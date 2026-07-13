//! Phase F Stage 4a — the platform-schema migrate path on the PUBLISHED engine.
//!
//! This module is the reusable core behind the `zeroship-platform-migrate` bin (and
//! the gated integration test). It rebuilds the retired in-tree `zeroship-migrate`
//! CLI's platform-apply behaviour entirely on the standalone `zero-migrate` engine:
//!
//! 1. discover `db/migrations-ts/*.ts` in filename order,
//! 2. AUTHOR each via zeroship-runtime's V8 + the standalone v1 recorder (the S2
//!    `author_v1_envelope` mechanism) → `{ ir_version:1, name, ops }`,
//! 3. LOWER each via `IrAuthor::load_and_lower_guarded` under a **Platform** guard
//!    (cross-schema `public`, the `citext`/`uuid-ossp` extension allowlist, roles /
//!    grants / functions — the widened operator posture the platform schema needs),
//! 4. APPLY each via `MigrationEngine::apply_plan_with_touched_and_depends_scoped`
//!    over `PostgresBackend::new_generic(&CompioPgSession)`, recording to the
//!    platform journal (`<project_schema>_migrations`).
//!
//! # The two operator-side Platform seams (LOWER + APPLY)
//!
//! Both halves run on the published engine's PUBLIC, token-gated Platform API:
//!
//! - LOWER — `GuardConfig::platform(&cap, schemas, extensions)` (already public):
//!   every op the platform schema uses — `schema` / `extension` / `role` /
//!   `domain` / `sequence` / `createFunction` / `raw` / `table().trigger()` /
//!   `table().comment()` / `column().comment()` / `setRls` / `policy()` /
//!   `currentSetting` / `grant` / `revoke` / `dropFunction` / `table().drop()` —
//!   authors on the standalone v1 recorder and lowers under the Platform guard.
//!   The DSL/op support is COMPLETE — there is NO missing op type.
//! - APPLY — `ExecutorConfig::platform(&cap, project_id, project_schema, schemas,
//!   extensions)`: the operator-side production seam for a Platform-trust executor
//!   (the APPLY-half peer of `GuardConfig::platform`). `MigrationEngine`'s executor
//!   derives its first-pass guard from `exec_cfg.guard_config()`, which honours
//!   `Platform` because the config was built through this token-gated ctor, so it
//!   admits the platform DDL (CREATE SCHEMA / roles / grants / cross-schema
//!   `public` / functions) the confined creator posture denies.
//!
//! Both require an `OperatorCapability` token, minted through the engine's named
//! production seam `OperatorCapability::new()`. This monorepo bin is the
//! operator-side production caller — the napi host is not the only legitimate
//! Platform-apply producer.

use std::path::{Path, PathBuf};

use zero_migrate::driver::SqlSession;
use zero_migrate::guard::GuardConfig;
use zero_migrate::{
    resolve_create_table_policy, Approval, ApprovalScope, ExecutorConfig, IrAuthor, LiveSchema,
    LockMode, MigrationEngine, MigrationIr, PolicyProfile, PostgresBackend, SqlDialect,
};
use zero_migrate_ir::capability::OperatorCapability;

use crate::CompioPgSession;

mod author;

/// The owner-app label stamped on every platform migration (ownership is enforced
/// UPSTREAM by the IR-load gate; for the platform schema the whole `zeroship`
/// schema is owned by this single synthetic platform app). Mirrors the in-tree
/// `PLATFORM_OWNER_APP` the retired CLI stamped.
const PLATFORM_OWNER_APP: &str = "zeroship_platform";

/// Inputs for a platform migrate run.
#[derive(Debug, Clone)]
pub struct PlatformMigrateConfig {
    /// Admin Postgres DSN (a superuser/owner on :5440 for platform DDL — roles,
    /// grants, RLS, functions).
    pub database_url: String,
    /// The `db/migrations-ts` directory holding the `*.ts` platform migrations.
    pub migrations_dir: PathBuf,
    /// The primary platform schema (conventionally `zeroship`).
    pub project_schema: String,
    /// The advisory-lock / journal project id (conventionally `zeroship`).
    pub project_id: String,
}

/// The cross-schema allowlist the Platform guard permits references to. The
/// platform migrations reference `public` (extensions, unqualified `citext`) beside
/// the primary `zeroship` schema.
fn platform_schemas(project_schema: &str) -> Vec<String> {
    vec![project_schema.to_string(), "public".to_string()]
}

/// The `CREATE EXTENSION` allowlist the platform migrations rely on.
fn platform_extensions() -> Vec<String> {
    vec!["citext".to_string(), "uuid-ossp".to_string()]
}

/// What a platform migrate run produced.
#[derive(Debug, Default)]
pub struct PlatformMigrateReport {
    /// Number of `.ts` files discovered + processed.
    pub files: usize,
    /// The versions/names newly applied this run.
    pub applied: Vec<String>,
    /// The versions/names skipped (already applied — idempotent re-run).
    pub skipped: Vec<String>,
}

/// A platform migrate error, attributed to a file where possible.
#[derive(Debug)]
pub enum PlatformMigrateError {
    /// A filesystem read of the migrations dir / a `.ts` file failed.
    Read { path: String, message: String },
    /// Connecting the compio session failed.
    Connect(String),
    /// Provisioning the primary schema failed.
    Provision(String),
    /// V8 authoring a `.ts` into a v1 envelope failed.
    Author { file: String, message: String },
    /// Folding the confined table-shape / resolving policy failed.
    Shape { file: String, message: String },
    /// The fail-closed load gate / guarded lower rejected the envelope. THIS is the
    /// arm that surfaces an engine op-support gap (an op the published engine
    /// genuinely cannot lower on Postgres).
    Lower { file: String, message: String },
    /// The engine's apply (executor guard / DDL execution / journal) failed.
    Apply { file: String, message: String },
    /// Reading the live schema snapshot failed.
    Snapshot(String),
}

impl std::fmt::Display for PlatformMigrateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Read { path, message } => write!(f, "read {path}: {message}"),
            Self::Connect(m) => write!(f, "connect: {m}"),
            Self::Provision(m) => write!(f, "provision schema: {m}"),
            Self::Author { file, message } => write!(f, "author {file}: {message}"),
            Self::Shape { file, message } => write!(f, "table-shape {file}: {message}"),
            Self::Lower { file, message } => {
                write!(f, "lower {file}: {message} (possible engine op-support gap)")
            }
            Self::Apply { file, message } => write!(f, "apply {file}: {message}"),
            Self::Snapshot(m) => write!(f, "snapshot live schema: {m}"),
        }
    }
}

impl std::error::Error for PlatformMigrateError {}

/// Live facts advanced between files: the table-ownership registry + the live
/// schema (so a later file sees tables created by an earlier one). Mirrors S3's
/// `PostgresIrApplyState`.
struct ApplyState {
    registry: std::collections::BTreeMap<String, String>,
    live_schema: LiveSchema,
}

/// Discover `db/migrations-ts/*.ts` files, deterministically ordered by filename.
fn discover_ts_files(dir: &Path) -> Result<Vec<PathBuf>, PlatformMigrateError> {
    let read = std::fs::read_dir(dir).map_err(|e| PlatformMigrateError::Read {
        path: dir.display().to_string(),
        message: e.to_string(),
    })?;
    let mut files: Vec<PathBuf> = Vec::new();
    for entry in read {
        let entry = entry.map_err(|e| PlatformMigrateError::Read {
            path: dir.display().to_string(),
            message: e.to_string(),
        })?;
        let path = entry.path();
        if path.is_file()
            && path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.ends_with(".ts"))
        {
            files.push(path);
        }
    }
    files.sort();
    Ok(files)
}

/// Derive the migration name label from a filename: strip the `.ts` extension and
/// the leading `NNNNNNNNNNNNNN_` timestamp prefix, leaving the human slug.
fn name_from_filename(path: &Path) -> String {
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("migration");
    match stem.split_once('_') {
        Some((prefix, rest)) if prefix.chars().all(|c| c.is_ascii_digit()) && !rest.is_empty() => {
            rest.to_string()
        }
        _ => stem.to_string(),
    }
}

/// Seed the apply state from the live project schema.
async fn seed_state(
    session: &CompioPgSession,
    project_schema: &str,
    owner_app: &str,
) -> Result<ApplyState, PlatformMigrateError> {
    let live = zero_migrate::snapshot_schema(session, project_schema)
        .await
        .map_err(|e| PlatformMigrateError::Snapshot(e.to_string()))?;
    let registry: std::collections::BTreeMap<String, String> = live
        .tables
        .keys()
        .map(|t| (t.clone(), owner_app.to_string()))
        .collect();
    let live_schema = LiveSchema {
        tables: live.tables.keys().cloned().collect(),
        unique_indexes: live
            .tables
            .values()
            .flat_map(|t| t.indexes.iter())
            .filter(|idx| idx.unique)
            .map(|idx| idx.name.clone())
            .collect(),
        table_snapshots: live.tables.clone(),
        partitions: live.partitions.clone(),
        table_ownership: live
            .tables
            .keys()
            .map(|t| (t.clone(), owner_app.to_string()))
            .collect(),
        sqlite_schemas: std::collections::BTreeMap::new(),
    };
    Ok(ApplyState {
        registry,
        live_schema,
    })
}

/// The immutable Platform authoring/lowering context threaded across all files.
struct LowerCtx {
    project_schema: String,
    owner_app: &'static str,
    guard_cfg: GuardConfig,
    policy_profile: PolicyProfile,
}

impl LowerCtx {
    fn new(project_schema: &str) -> Self {
        let cap = OperatorCapability::new();
        let schemas = platform_schemas(project_schema);
        let extensions = platform_extensions();
        Self {
            project_schema: project_schema.to_string(),
            owner_app: PLATFORM_OWNER_APP,
            guard_cfg: GuardConfig::platform(&cap, schemas, extensions),
            policy_profile: PolicyProfile::platform(),
        }
    }
}

/// AUTHOR (V8, v1 recorder) + LOWER (fail-closed load gate + guarded lower under
/// the Platform guard) ONE `.ts` file against the current live `state`. This is the
/// half of the pipeline that runs entirely on the published engine and needs NO
/// privileged executor seam — so it is fully reachable + provable in the monorepo.
///
/// Returns the lowered artifact (plan steps + touched/created tables).
fn author_and_lower_file(
    ctx: &LowerCtx,
    state: &ApplyState,
    path: &Path,
) -> Result<zero_migrate::render::lower::LoweredArtifact, PlatformMigrateError> {
    let file = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("<unknown>")
        .to_string();

    // (1) read the `.ts` source.
    let source = std::fs::read_to_string(path).map_err(|e| PlatformMigrateError::Read {
        path: file.clone(),
        message: e.to_string(),
    })?;

    // (2) AUTHOR the v1 envelope in zeroship-runtime's V8 (S2 mechanism).
    let name = name_from_filename(path);
    let envelope =
        author::author_v1_envelope(&source, &name).map_err(|message| PlatformMigrateError::Author {
            file: file.clone(),
            message,
        })?;

    // (3) fold the Platform table-shape profile into every createTable BEFORE the
    // fail-closed load gate. Non-createTable ops pass through untouched.
    let bytes = resolve_shape(&envelope, &ctx.policy_profile, &file)?;

    // (4) fail-closed load gate + guarded lower under the Platform guard.
    let ir_author = IrAuthor::new(&ctx.project_schema, ctx.owner_app, SqlDialect::Postgres);
    let ir_author = match ctx.guard_cfg.schema_scope() {
        Some(scope) => ir_author.with_schema_scope(scope),
        None => ir_author,
    };
    ir_author
        .load_and_lower_guarded(
            &bytes,
            ctx.owner_app,
            &state.registry,
            &state.live_schema,
            &ctx.guard_cfg,
            Some(&ctx.policy_profile),
        )
        .map_err(|e| PlatformMigrateError::Lower {
            file,
            message: e.to_string(),
        })
}

/// Fold the tables an artifact creates into the live state so a later file sees
/// them (the cross-file registry + live-set advance).
fn advance_state(state: &mut ApplyState, owner_app: &str, created_tables: &[String]) {
    for t in created_tables {
        state
            .registry
            .entry(t.clone())
            .or_insert_with(|| owner_app.to_string());
        state.live_schema.tables.insert(t.clone());
    }
}

/// AUTHOR + LOWER every `db/migrations-ts/*.ts` file in order, threading the live
/// state across files, WITHOUT applying. This is the fully-reachable proof half of
/// the pipeline: it exercises the V8 authoring + the published engine's
/// fail-closed load gate + Platform-guarded lower for every platform migration,
/// over an EMPTY starting `state` (no DB required). Returns the per-file lowered
/// artifacts so a caller can inspect the generated plan steps.
///
/// Used by the gated integration test to assert all 11 platform migrations author
/// + lower cleanly on the standalone (v1) engine.
pub fn author_and_lower_all(
    migrations_dir: &Path,
    project_schema: &str,
) -> Result<Vec<(String, zero_migrate::render::lower::LoweredArtifact)>, PlatformMigrateError> {
    let files = discover_ts_files(migrations_dir)?;
    let ctx = LowerCtx::new(project_schema);
    // Empty live state: authoring + lowering do not consult the DB. Cross-file
    // table references (e.g. functions/triggers/grants targeting tables created in
    // an earlier file) resolve through the advancing registry/live-set.
    let mut state = ApplyState {
        registry: std::collections::BTreeMap::new(),
        live_schema: LiveSchema::default(),
    };
    let mut out = Vec::with_capacity(files.len());
    for path in &files {
        let file = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("<unknown>")
            .to_string();
        let lowered = author_and_lower_file(&ctx, &state, path)?;
        advance_state(&mut state, ctx.owner_app, &lowered.created_tables);
        out.push((file, lowered));
    }
    Ok(out)
}

/// Run all platform migrations end to end. Holds the project advisory lock across
/// the whole set (acquire on the first file, already-held for the rest).
pub async fn run_platform_migrations(
    cfg: &PlatformMigrateConfig,
) -> Result<PlatformMigrateReport, PlatformMigrateError> {
    let files = discover_ts_files(&cfg.migrations_dir)?;

    // ── driver: open a native compio session + provision the primary schema ──
    let session = CompioPgSession::connect(&cfg.database_url)
        .await
        .map_err(|e| PlatformMigrateError::Connect(e.to_string()))?;
    session
        .batch(&format!(
            "CREATE SCHEMA IF NOT EXISTS \"{}\"",
            cfg.project_schema
        ))
        .await
        .map_err(|e| PlatformMigrateError::Provision(e.to_string()))?;

    let ctx = LowerCtx::new(&cfg.project_schema);
    let owner_app = ctx.owner_app;

    // ── the Platform executor posture (operator-side production seam) ──
    // The engine's executor first-pass guard is derived from
    // `exec_cfg.guard_config()`, which honours Platform ONLY when the config was
    // built via the token-gated `ExecutorConfig::platform`. That ctor is the
    // PUBLIC operator-side Platform seam (the APPLY-half peer of the already-public
    // `GuardConfig::platform` LOWER-half seam); it requires an `OperatorCapability`
    // token, minted here through the engine's named production seam
    // `OperatorCapability::new()`. This monorepo bin is the operator-side
    // production caller — it applies the platform's own trusted infra schema
    // (CREATE SCHEMA / roles / grants / cross-schema public / functions) over the
    // native compio `SqlSession`, so the executor guard admits platform DDL.
    let exec_cap = OperatorCapability::new();
    let exec_cfg = ExecutorConfig::platform(
        &exec_cap,
        cfg.project_id.clone(),
        cfg.project_schema.clone(),
        platform_schemas(&cfg.project_schema),
        platform_extensions(),
    );

    let backend = PostgresBackend::new_generic(&session);
    let engine = MigrationEngine::new();

    let mut state = seed_state(&session, &cfg.project_schema, owner_app).await?;
    let mut report = PlatformMigrateReport {
        files: files.len(),
        ..Default::default()
    };

    for (index, path) in files.iter().enumerate() {
        let file = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("<unknown>")
            .to_string();

        // AUTHOR (V8) + LOWER (Platform guard) — the fully-reachable half.
        let lowered = author_and_lower_file(&ctx, &state, path)?;
        let created_tables = lowered.created_tables.clone();

        // APPLY over the native compio seam, holding the project lock across the
        // whole set. Under the reachable Confined executor this fail-closes at the
        // first platform-DDL op — the surfaced engine gap.
        let lock_mode = if index == 0 {
            LockMode::Acquire
        } else {
            LockMode::AlreadyHeld
        };
        let outcome = engine
            .apply_plan_with_touched_and_depends_scoped(
                &lowered.plan.steps,
                &lowered.touched_tables,
                &lowered.depends_on,
                // Operator-side unattended platform apply: the committed platform
                // schema is trusted, and a destructive migration (e.g.
                // `drop_metering_exports` DROP TABLE … CASCADE) is a deliberate,
                // reviewed part of that committed set. This is the docker-compose
                // one-shot posture — auto-approved, scope = all — matching the
                // retired in-tree CLI's platform-apply behaviour.
                Approval::Approved,
                &ApprovalScope::All,
                &backend,
                &exec_cfg,
                "phase-f-stage4a",
                lock_mode,
                None,
            )
            .await
            .map_err(|e| PlatformMigrateError::Apply {
                file: file.clone(),
                message: e.to_string(),
            })?;

        report.applied.extend(outcome.applied.applied);
        report.skipped.extend(outcome.applied.skipped);
        advance_state(&mut state, owner_app, &created_tables);
    }

    Ok(report)
}

/// Fold the Platform table-shape profile into an envelope's createTable ops.
fn resolve_shape(
    envelope: &str,
    profile: &PolicyProfile,
    file: &str,
) -> Result<String, PlatformMigrateError> {
    let ir: MigrationIr =
        serde_json::from_str(envelope).map_err(|e| PlatformMigrateError::Shape {
            file: file.to_string(),
            message: format!("deserialize IR envelope: {e}"),
        })?;
    let resolved =
        resolve_create_table_policy(&ir, profile).map_err(|e| PlatformMigrateError::Shape {
            file: file.to_string(),
            message: format!("resolve table-shape policy: {e}"),
        })?;
    serde_json::to_string(&resolved).map_err(|e| PlatformMigrateError::Shape {
        file: file.to_string(),
        message: format!("re-serialize resolved IR envelope: {e}"),
    })
}
