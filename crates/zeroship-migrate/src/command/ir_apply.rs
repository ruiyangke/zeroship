//! **PR7 online-rename go-live SEAM — SQLite leg (engine-wired, deploy-handler
//! deferred; NOT an end-to-end production path).** The library entry point that
//! applies a bundle's `.ir.json` creator artifacts against a **SQLite** backend —
//! the SQLite peer of control's PG `apply_bundle_ir_migrations`
//! (`crates/control/src/deploy_migrate.rs`).
//!
//! TRUTH-IN-LABELING (code-critic Q3). This is engine/library-level wiring, NOT a
//! deployable rename. There is NO production caller: the SQLite rename path is
//! **structurally test-only** — a production caller derives descriptors from
//! `registerModel` = the POST-deploy desired schema, in which the rename's PRE-rename
//! `from` column no longer exists, so the rebuild author fails CLOSED (zero data
//! loss) but cannot run. The GREEN SQLite tests pass only because they hand-feed a
//! PRE-rename / INTERMEDIATE descriptor shape no production caller possesses (see the
//! PRODUCTION-WIRING TODO below). The test-only status is LOAD-BEARING and pinned by
//! the control interlock regression
//! (`production_deploy_handler_never_wires_the_unguarded_approved_go_live_surface`):
//! wiring this before the §2.0.3 cross-deploy pending-contract interlock is
//! persisted/enforced would open a real hazard.
//!
//! Before PR7 there was NO production/dev path that built a SQLite-dialect
//! [`LiveSchema`] (the `table_snapshots` + `sqlite_schemas` SDK-`Value` facts a
//! SQLite `renameColumn` rebuild needs), so a SQLite-targeted IR rename was
//! engine-proven (PR2 unit + temp-file e2e) but never go-live-wired: it failed
//! closed before lowering. This module wires the SEAM (engine library surface); the
//! deploy-handler call site is deferred to the SQLite go-live wave (TODO below).
//!
//! The SDK schema `Value` the SQLite rebuild renders the post-rename `CREATE TABLE`
//! from is NOT recoverable from a raw `sqlite_master` introspection (the
//! mask/encryption/ref facets live only in the SDK schema, not the catalog). The
//! authoritative source is a descriptor set, threaded in by the caller;
//! [`LiveSchema::for_sqlite_descriptors`] routes it through the SAME
//! `desired_snapshot_for_dialect` the differ uses, so the live facts the rename
//! rebuild consumes are byte-identical to a `t.*`-diff's.
//!
//! TWO ROLES, OPPOSITE STATE (see the DESCRIPTOR-SET CONTRACT note in
//! [`apply_bundle_ir_sqlite`]): the descriptor set is the union END-STATE for the
//! ownership/FK-inline registry, but a `renameColumn` rebuild needs the PRE-rename
//! LIVE column facts (the `from` column must be present). A descriptor set derived
//! from the app's registerModel = the POST-deploy DESIRED schema (post-rename `from`
//! is gone) makes a rename FAIL CLOSED (no data loss) but un-runnable.
//!
//! PRODUCTION-WIRING TODO (the SQLite go-live wave): a production caller naturally
//! derives descriptors from registerModel (the post-deploy desired schema), so it
//! MUST source the rename's PRE-rename column facts from a real pre-deploy
//! SQLite-catalog/snapshot read (the SDK-`Value` facets reconstructed at deploy
//! start), OR carry the pre-rename column in the IR — NOT from the post-deploy
//! desired descriptor set. Until wired, this surface is test-only (no production
//! caller; pinned by the control interlock regression test).
//!
//! Every `.ir.json` runs through the SAME fail-closed load + guard-per-fragment
//! lower gate (`IrAuthor::load_and_lower_guarded`) the PG path uses, then is applied
//! through the single shared `apply_plan` — so the SQLite leg inherits the IR-load
//! ownership/version/checksum-hint defenses and the per-op guard, identically.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::apply::backend::{MigrationBackend, PostgresBackend};
use crate::apply::drift::DriftError;
use crate::apply::executor::{ApplyError, LockMode};
use crate::apply::journal::{DeployRecoveryScope, PendingContract};
use crate::approval::ApprovalScope;
use crate::frontend::{record_migration_transient, BuildError, DiscoveredMigration, RecordVia};
use crate::PolicyProfile;
use crate::model::migration::Migration;
use crate::model::profile::{SealError, SealVerifier, SealedProfile};
use crate::render::step::PlanStep;
use crate::render::declarative::CollectionDescriptor;
use crate::render::lower::{IrAuthor, LiveSchema, LoadAndLowerGuardedError};
use crate::{
    Approval, DeclarativeApplyError, DeclarativeError, EngineError, ExecutorConfig, GuardConfig,
    MigrationEngine, SqlDialect, SqliteBackend,
};

/// Discover `*.ir.json` files in a migration directory, ordered deterministically
/// by path.
///
/// An empty directory, or one with no IR artifacts, returns an empty vector. The
/// caller is responsible for deciding whether non-IR siblings are legal for that
/// command/profile; this helper is intentionally format-neutral so the SQLite,
/// control-plane PG, and Platform PG paths share the same IR discovery.
///
/// # Errors
/// [`IrDiscoveryError`] when the directory cannot be read.
pub fn discover_ir_files(migrations_dir: &Path) -> Result<Vec<PathBuf>, IrDiscoveryError> {
    let mut ir_files: Vec<PathBuf> = Vec::new();
    let read = std::fs::read_dir(migrations_dir).map_err(|e| IrDiscoveryError::Read {
        file: migrations_dir.display().to_string(),
        message: e.to_string(),
    })?;
    for entry in read {
        let entry = entry.map_err(|e| IrDiscoveryError::Read {
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
    ir_files.sort();
    Ok(ir_files)
}

/// A failure while discovering `*.ir.json` migration artifacts.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum IrDiscoveryError {
    /// Reading the directory failed.
    #[error("read IR file ({file}): {message}")]
    Read {
        /// The path/filename.
        file: String,
        /// The I/O error.
        message: String,
    },
}

/// Mutable live facts the PG `.ir.json` apply loop advances between files.
#[derive(Debug, Clone)]
pub struct PostgresIrApplyState {
    /// Table ownership registry consumed by the fail-closed IR load gate.
    pub registry: BTreeMap<String, String>,
    /// Live schema facts consumed by the IR lowerer.
    pub live_schema: LiveSchema,
}

/// What a PG IR apply produced.
#[derive(Debug, Clone, Default)]
pub struct PostgresIrApplyOutcome {
    /// Migration version ids applied this run.
    pub applied: Vec<String>,
    /// Migration version ids skipped (already journaled).
    pub skipped: Vec<String>,
    /// Deferred contract migration versions surfaced by completed PG online renames.
    pub pending_contract: Vec<String>,
    /// Full pending-contract records opened by this run.
    pub opened_obligations: Vec<PendingContract>,
    /// Lowered journal migrations, for set-level manifest/traceability callers.
    pub lowered_migrations: Vec<Migration>,
}

impl PostgresIrApplyOutcome {
    fn extend(&mut self, other: PostgresIrApplyOutcome) {
        self.applied.extend(other.applied);
        self.skipped.extend(other.skipped);
        self.pending_contract.extend(other.pending_contract);
        self.opened_obligations.extend(other.opened_obligations);
        self.lowered_migrations.extend(other.lowered_migrations);
    }
}

/// A failure applying a bundle's `.ir.json` set against a Postgres backend.
#[derive(Debug, thiserror::Error)]
pub enum PostgresIrApplyError {
    /// Reading the migrations directory / a `.ir.json` file failed.
    #[error("read IR file ({file}): {message}")]
    Read {
        /// The path/filename.
        file: String,
        /// The I/O error.
        message: String,
    },
    /// Introspecting the live schema failed.
    #[error("read Postgres catalog for live facts: {0}")]
    Snapshot(#[from] DriftError),
    /// A `.ir.json` failed the fail-closed LOAD GATE or guarded lower.
    #[error("IR load/guarded-lower ({file}): {source}")]
    Ir {
        /// The `.ir.json` filename.
        file: String,
        /// The fail-closed gate / guard error.
        #[source]
        source: LoadAndLowerGuardedError,
    },
    /// Recording a trusted Platform `.ts` migration failed before transient IR
    /// apply. The committed-IR creator path is unaffected.
    #[error("record platform TS migration ({file}): {source}")]
    Record {
        /// The `.ts` filename or directory.
        file: String,
        /// The recorder/build-front-end error.
        #[source]
        source: BuildError,
    },
    /// Platform `.ts` migrations use the 14-digit filename prefix as the corpus
    /// order key; duplicates are ambiguous and refused before any apply.
    #[error("duplicate platform TS migration version {version}: files '{first}' and '{second}'")]
    DuplicateTsVersion {
        /// The duplicated 14-digit version prefix.
        version: String,
        /// The first file seen with this version.
        first: String,
        /// The second file seen with this version.
        second: String,
    },
    /// The engine refused or failed the apply.
    #[error("apply: {0}")]
    Apply(#[from] DeclarativeApplyError),
}

/// A failure on the sealed shared-infra apply path.
#[derive(Debug, thiserror::Error)]
pub enum SealedApplyError {
    /// The sealed profile did not authenticate or was stale.
    #[error("sealed migration policy profile refused: {0}")]
    Seal(#[from] SealError),
    /// The existing guarded IR apply path failed after seal verification.
    #[error(transparent)]
    Apply(#[from] PostgresIrApplyError),
}

impl From<IrDiscoveryError> for PostgresIrApplyError {
    fn from(error: IrDiscoveryError) -> Self {
        match error {
            IrDiscoveryError::Read { file, message } => Self::Read { file, message },
        }
    }
}

impl From<IrDiscoveryError> for SqliteIrApplyError {
    fn from(error: IrDiscoveryError) -> Self {
        match error {
            IrDiscoveryError::Read { file, message } => Self::Read { file, message },
        }
    }
}

/// Seed the PG IR apply state from the live project schema.
///
/// The registry and live-table facts are then advanced after each applied IR file
/// with that file's `createTable` set, matching the control-plane deploy path's
/// cross-file behavior.
///
/// # Errors
/// [`DriftError`] if live schema introspection fails.
pub async fn postgres_ir_apply_state(
    backend: &PostgresBackend<'_>,
    exec_cfg: &ExecutorConfig,
    owner_app: &str,
) -> Result<PostgresIrApplyState, DriftError> {
    let live = backend.snapshot_schema(exec_cfg).await?;
    let registry: BTreeMap<String, String> = live
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
        table_ownership: live
            .tables
            .keys()
            .map(|t| (t.clone(), owner_app.to_string()))
            .collect(),
        sqlite_schemas: BTreeMap::new(),
    };
    Ok(PostgresIrApplyState {
        registry,
        live_schema,
    })
}

/// Apply all `*.ir.json` files in a directory to Postgres using an already-built
/// executor/guard profile.
///
/// This is the Platform-profile entry point used by the CLI runner. It discovers
/// IR artifacts, seeds live facts from the current catalog, holds the project
/// advisory lock once across the whole file set, and applies each lowered plan via
/// the shared `MigrationEngine::apply_plan_with_touched_and_depends_scoped`.
///
/// `project_schema` is the default schema for unqualified IR ops. `owner_app` is
/// the server-stamped owner used by the IR load gate; Platform callers pass the
/// fixed platform owner sentinel, while confined deploy callers use the app id.
///
/// # Errors
/// [`PostgresIrApplyError`] on discovery, live introspection, guarded lower, or
/// apply failure.
#[allow(clippy::too_many_arguments)]
pub async fn apply_bundle_ir_postgres(
    backend: &PostgresBackend<'_>,
    project_schema: &str,
    owner_app: &str,
    migrations_dir: &Path,
    exec_cfg: &ExecutorConfig,
    guard_cfg: &GuardConfig,
    approval: Approval,
    applied_by: &str,
) -> Result<PostgresIrApplyOutcome, PostgresIrApplyError> {
    let ir_files = discover_ir_files(migrations_dir)?;
    if ir_files.is_empty() {
        return Ok(PostgresIrApplyOutcome::default());
    }
    let mut state = postgres_ir_apply_state(backend, exec_cfg, owner_app).await?;

    backend
        .acquire_project_lock(exec_cfg)
        .await
        .map_err(|e| PostgresIrApplyError::Apply(DeclarativeApplyError::Plain(
            EngineError::Apply(e),
        )))?;

    let mut outcome = PostgresIrApplyOutcome::default();
    let body = async {
        for path in &ir_files {
            let file_outcome = apply_one_ir_file_postgres(
                backend,
                project_schema,
                owner_app,
                path,
                &mut state,
                exec_cfg,
                guard_cfg,
                approval,
                &ApprovalScope::All,
                applied_by,
                LockMode::AlreadyHeld,
                None,
            )
            .await?;
            outcome.extend(file_outcome);
        }
        Ok::<(), PostgresIrApplyError>(())
    }
    .await;

    let unlock = backend.release_project_lock(exec_cfg).await;
    match (body, unlock) {
        (Ok(()), Ok(())) => Ok(outcome),
        (Ok(()), Err(e)) => Err(PostgresIrApplyError::Apply(
            DeclarativeApplyError::Plain(EngineError::Apply(e)),
        )),
        (Err(e), unlock_result) => {
            if let Err(unlock_err) = unlock_result {
                tracing::warn!(
                    error = %unlock_err,
                    project = %exec_cfg.project_id,
                    "zeroship-migrate: failed to release project lock after PG IR apply"
                );
            }
            Err(e)
        }
    }
}

/// Apply an `.ir.json` bundle to Postgres through a sealed shared-infra policy
/// profile.
///
/// Stage M2-1 deliberately delegates to the existing `GuardConfig` apply path
/// after verifying the in-process MAC and ceiling version. The sealed profile can
/// lower only to today's `Confined` or `Platform` guard; `Trusted` is not
/// representable in [`SealedProfile`].
///
/// # Errors
/// [`SealedApplyError::Seal`] when the seal is invalid or stale, otherwise the
/// underlying [`PostgresIrApplyError`] from [`apply_bundle_ir_postgres`].
#[allow(clippy::too_many_arguments)]
pub async fn apply_sealed(
    backend: &PostgresBackend<'_>,
    sealed: SealedProfile,
    verifier: &SealVerifier,
    owner_app: &str,
    migrations_dir: &Path,
    exec_cfg: &ExecutorConfig,
    approval: Approval,
    applied_by: &str,
) -> Result<PostgresIrApplyOutcome, SealedApplyError> {
    sealed.verify(verifier)?;
    sealed.reconcile_with_executor_config(exec_cfg)?;
    let guard_cfg = sealed.to_guard_config();
    let mut exec_cfg = exec_cfg.clone();
    sealed.apply_operational_to_executor_config(&mut exec_cfg);
    apply_bundle_ir_postgres(
        backend,
        sealed.project_schema(),
        owner_app,
        migrations_dir,
        &exec_cfg,
        &guard_cfg,
        approval,
        applied_by,
    )
    .await
    .map_err(SealedApplyError::Apply)
}

/// Raw standalone Postgres IR convenience for user-owned databases.
///
/// This is intentionally absent from default/server builds. It takes a raw
/// [`GuardConfig`] and performs no seal verification; only the standalone CLI
/// feature may link it.
#[cfg(feature = "standalone-cli")]
#[allow(clippy::too_many_arguments)]
pub async fn apply_standalone(
    backend: &PostgresBackend<'_>,
    project_schema: &str,
    owner_app: &str,
    migrations_dir: &Path,
    exec_cfg: &ExecutorConfig,
    guard_cfg: &GuardConfig,
    approval: Approval,
    applied_by: &str,
) -> Result<PostgresIrApplyOutcome, PostgresIrApplyError> {
    apply_bundle_ir_postgres(
        backend,
        project_schema,
        owner_app,
        migrations_dir,
        exec_cfg,
        guard_cfg,
        approval,
        applied_by,
    )
    .await
}

/// Apply a Platform `.ts` migration corpus to Postgres by recording each source
/// file to transient IR at migrate time, then feeding that IR through the same
/// guarded lower + apply core as IR bundle apply.
///
/// This is Model C for trusted platform authors: `.ts` is the committed source of
/// truth, `.ir.json` is never written, and the existing sandboxed recorder is reused
/// as the TypeScript front-end. The project advisory lock is held once across the
/// corpus, matching [`apply_bundle_ir_postgres`].
#[allow(clippy::too_many_arguments)]
pub async fn apply_platform_ts_postgres(
    backend: &PostgresBackend<'_>,
    project_schema: &str,
    owner_app: &str,
    migrations_dir: &Path,
    record_via: &RecordVia<'_>,
    exec_cfg: &ExecutorConfig,
    guard_cfg: &GuardConfig,
    approval: Approval,
    applied_by: &str,
) -> Result<PostgresIrApplyOutcome, PostgresIrApplyError> {
    let ts_migrations = crate::frontend::discover_migrations(migrations_dir).map_err(|source| {
        PostgresIrApplyError::Record {
            file: migrations_dir.display().to_string(),
            source,
        }
    })?;
    if ts_migrations.is_empty() {
        return Ok(PostgresIrApplyOutcome::default());
    }
    reject_duplicate_ts_versions(&ts_migrations)?;

    let mut state = postgres_ir_apply_state(backend, exec_cfg, owner_app).await?;

    backend
        .acquire_project_lock(exec_cfg)
        .await
        .map_err(|e| PostgresIrApplyError::Apply(DeclarativeApplyError::Plain(
            EngineError::Apply(e),
        )))?;

    let mut outcome = PostgresIrApplyOutcome::default();
    let body = async {
        for migration in &ts_migrations {
            let display = migration
                .ts_path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("<unknown>")
                .to_string();
            let recorded =
                record_migration_transient(migration, owner_app, record_via, &PolicyProfile::platform())
                .map_err(|source| PostgresIrApplyError::Record {
                    file: display.clone(),
                    source,
                })?;
            debug_assert_eq!(recorded.stem, migration.stem);
            debug_assert_eq!(recorded.version, migration.version);

            let file_outcome = apply_one_ir_document_postgres(
                backend,
                project_schema,
                owner_app,
                &display,
                &recorded.ir_json,
                &mut state,
                exec_cfg,
                guard_cfg,
                approval,
                &ApprovalScope::All,
                applied_by,
                LockMode::AlreadyHeld,
                None,
                Some(PlatformTsVersionSeed {
                    version: &migration.version,
                }),
            )
            .await?;
            outcome.extend(file_outcome);
        }
        Ok::<(), PostgresIrApplyError>(())
    }
    .await;

    let unlock = backend.release_project_lock(exec_cfg).await;
    match (body, unlock) {
        (Ok(()), Ok(())) => Ok(outcome),
        (Ok(()), Err(e)) => Err(PostgresIrApplyError::Apply(
            DeclarativeApplyError::Plain(EngineError::Apply(e)),
        )),
        (Err(e), unlock_result) => {
            if let Err(unlock_err) = unlock_result {
                tracing::warn!(
                    error = %unlock_err,
                    project = %exec_cfg.project_id,
                    "zeroship-migrate: failed to release project lock after Platform TS apply"
                );
            }
            Err(e)
        }
    }
}

/// Apply one PG `.ir.json` file using caller-managed state and lock mode.
///
/// The control-plane deploy path calls this inside its existing whole-deploy lock
/// and recovery bracket; the Platform runner calls it through
/// [`apply_bundle_ir_postgres`]. This is the shared load + guarded-lower +
/// `apply_plan` core for PG IR artifacts.
#[allow(clippy::too_many_arguments)]
pub async fn apply_one_ir_file_postgres(
    backend: &PostgresBackend<'_>,
    project_schema: &str,
    owner_app: &str,
    path: &Path,
    state: &mut PostgresIrApplyState,
    exec_cfg: &ExecutorConfig,
    guard_cfg: &GuardConfig,
    approval: Approval,
    scope: &ApprovalScope,
    applied_by: &str,
    lock_mode: LockMode,
    recovery_scope: Option<&DeployRecoveryScope<'_>>,
) -> Result<PostgresIrApplyOutcome, PostgresIrApplyError> {
    let file = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("<unknown>")
        .to_string();
    let bytes = std::fs::read_to_string(path).map_err(|e| PostgresIrApplyError::Read {
        file: file.clone(),
        message: e.to_string(),
    })?;

    apply_one_ir_document_postgres(
        backend,
        project_schema,
        owner_app,
        &file,
        &bytes,
        state,
        exec_cfg,
        guard_cfg,
        approval,
        scope,
        applied_by,
        lock_mode,
        recovery_scope,
        None,
    )
    .await
}

#[derive(Debug, Clone, Copy)]
struct PlatformTsVersionSeed<'a> {
    version: &'a str,
}

#[allow(clippy::too_many_arguments)]
async fn apply_one_ir_document_postgres(
    backend: &PostgresBackend<'_>,
    project_schema: &str,
    owner_app: &str,
    file: &str,
    bytes: &str,
    state: &mut PostgresIrApplyState,
    exec_cfg: &ExecutorConfig,
    guard_cfg: &GuardConfig,
    approval: Approval,
    scope: &ApprovalScope,
    applied_by: &str,
    lock_mode: LockMode,
    recovery_scope: Option<&DeployRecoveryScope<'_>>,
    platform_ts_seed: Option<PlatformTsVersionSeed<'_>>,
) -> Result<PostgresIrApplyOutcome, PostgresIrApplyError> {
    let mut author = IrAuthor::new(project_schema, owner_app, SqlDialect::Postgres);
    if let Some(scope) = guard_cfg.schema_scope() {
        author = author.with_schema_scope(scope);
    }
    let mut lowered = author
        .load_and_lower_guarded(
            bytes,
            owner_app,
            &state.registry,
            &state.live_schema,
            guard_cfg,
        )
        .map_err(|source| PostgresIrApplyError::Ir {
            file: file.to_string(),
            source,
        })?;
    if let Some(seed) = platform_ts_seed {
        restamp_platform_ts_ddl_versions(&mut lowered, seed);
    }

    let lowered_migrations = lowered.migrations();
    let created_tables = lowered.created_tables.clone();
    let outcome = MigrationEngine::new()
        .apply_plan_with_touched_and_depends_scoped(
            &lowered.plan.steps,
            &lowered.touched_tables,
            &lowered.depends_on,
            approval,
            scope,
            backend,
            exec_cfg,
            applied_by,
            lock_mode,
            recovery_scope,
        )
        .await?;

    for t in created_tables {
        state
            .registry
            .entry(t.clone())
            .or_insert_with(|| owner_app.to_string());
        state.live_schema.tables.insert(t);
    }

    Ok(PostgresIrApplyOutcome {
        applied: outcome.applied.applied,
        skipped: outcome.applied.skipped,
        pending_contract: outcome
            .pending_contract
            .iter()
            .map(|m| m.version.as_str().to_string())
            .collect(),
        opened_obligations: outcome.opened_obligations,
        lowered_migrations,
    })
}

fn reject_duplicate_ts_versions(
    migrations: &[DiscoveredMigration],
) -> Result<(), PostgresIrApplyError> {
    let mut seen: BTreeMap<&str, &DiscoveredMigration> = BTreeMap::new();
    for migration in migrations {
        if let Some(first) = seen.insert(&migration.version, migration) {
            return Err(PostgresIrApplyError::DuplicateTsVersion {
                version: migration.version.clone(),
                first: first
                    .ts_path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("<unknown>")
                    .to_string(),
                second: migration
                    .ts_path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("<unknown>")
                    .to_string(),
            });
        }
    }
    Ok(())
}

fn restamp_platform_ts_ddl_versions(
    lowered: &mut crate::render::lower::LoweredArtifact,
    seed: PlatformTsVersionSeed<'_>,
) {
    let file_version = seed
        .version
        .parse::<u64>()
        .expect("DiscoveredMigration already validated a 14-digit version");
    let mut remap = BTreeMap::new();
    let mut ddl_index = 0usize;
    for step in &mut lowered.plan.steps {
        let PlanStep::Ddl(migration) = step else {
            continue;
        };
        let new_version = platform_ts_ddl_fragment_id(file_version, ddl_index);
        remap.insert(migration.version.clone(), new_version.clone());
        migration.version = new_version;
        ddl_index += 1;
    }
    if remap.is_empty() {
        return;
    }
    for step in &mut lowered.plan.steps {
        let PlanStep::Ddl(migration) = step else {
            continue;
        };
        for dep in &mut migration.depends_on {
            if let Some(new_version) = remap.get(dep) {
                *dep = new_version.clone();
            }
        }
    }
}

fn platform_ts_ddl_fragment_id(
    file_version: u64,
    ddl_index: usize,
) -> crate::model::migration::MigrationId {
    debug_assert!(
        file_version < crate::model::migration::VERSION_CEILING,
        "DiscoveredMigration already validated a 14-digit version"
    );
    let ddl_index = u64::try_from(ddl_index).expect("DDL fragment count fits in u64");
    let mut bytes = [0u8; 16];
    bytes[0..6].copy_from_slice(&file_version.to_be_bytes()[2..8]);
    bytes[8..16].copy_from_slice(&ddl_index.to_be_bytes());
    let uuid = uuid::Uuid::from_bytes(bytes);
    crate::model::migration::MigrationId::parse(&format!(
        "mig_{}",
        zeroship_core::typed_id::uuid_to_base62(&uuid)
    ))
    .expect("derived Platform TS DDL fragment id is a valid mig_ typed id")
}

impl From<ApplyError> for PostgresIrApplyError {
    fn from(error: ApplyError) -> Self {
        Self::Apply(DeclarativeApplyError::Plain(EngineError::Apply(error)))
    }
}

/// What a SQLite IR apply produced.
#[derive(Debug, Clone, Default)]
pub struct SqliteIrApplyOutcome {
    /// Migration version ids applied this run.
    pub applied: Vec<String>,
    /// Migration version ids skipped (already journaled).
    pub skipped: Vec<String>,
}

/// A failure applying a bundle's `.ir.json` set against a SQLite backend.
#[derive(Debug, thiserror::Error)]
pub enum SqliteIrApplyError {
    /// Reading the migrations directory / a `.ir.json` file failed.
    #[error("read IR file ({file}): {message}")]
    Read {
        /// The path/filename.
        file: String,
        /// The I/O error.
        message: String,
    },
    /// Building the SQLite-dialect live schema from the descriptor set failed (an
    /// invalid field/type token rejected at the author boundary).
    #[error("build SQLite live schema: {0}")]
    LiveSchema(#[source] DeclarativeError),
    /// **PR9b** — reading the live SQLite catalog (`sqlite_master` + PRAGMA) to source
    /// the rename rebuild's PRE-rename column facts failed
    /// ([`apply_bundle_ir_sqlite_catalog`] via [`LiveSchema::from_sqlite_catalog`]).
    #[error("read SQLite catalog for live facts: {0}")]
    Catalog(#[source] crate::DriftError),
    /// A `.ir.json` failed the fail-closed LOAD GATE or its guard-per-fragment lower
    /// (malformed, future `ir_version`, ownership / structural reject, a guard-denied
    /// fragment with op-index attribution). A creator fault.
    #[error("IR load/guarded-lower ({file}): {source}")]
    Ir {
        /// The `.ir.json` filename.
        file: String,
        /// The fail-closed gate / guard error.
        #[source]
        source: LoadAndLowerGuardedError,
    },
    /// The engine refused or failed the apply (a guard denial, a destructive op /
    /// approval-gated rebuild without approval, checksum drift, or a mid-apply DB
    /// error).
    #[error("apply: {0}")]
    Apply(#[source] DeclarativeApplyError),
}

/// Apply a bundle's `.ir.json` creator artifacts against a **SQLite** backend
/// (§2.6.2 SQLite leg). Discovers `*.ir.json` files in `migrations_dir`
/// (version-ordered by filename), builds the SQLite-dialect [`LiveSchema`] from the
/// app's `descriptors`, then for each file runs the fail-closed load + guard lower
/// (`IrAuthor::load_and_lower_guarded`, SQLite dialect) and applies the resulting
/// plan through the single shared `apply_plan`.
///
/// A SQLite `renameColumn` lowers to an OFFLINE 12-step rebuild
/// (`RenameStep::SqliteRebuild`), applied via `MigrationBackend::rebuild_one` — there
/// is NO `pending_contract` partition on the SQLite leg (that is PG-only). A rebuild
/// on a populated table is destructive, so a rename apply requires
/// `Approval::Approved`; the routine `Approval::None` path refuses it before any DDL.
///
/// `project_schema` is the project id (the SQLite `main`-unqualified emit ignores
/// the qualifier but the shared emitter still validates it). `owner_app` stamps the
/// live-table ownership the differ's cross-app rebuild guard consults — every live
/// table in the app's descriptor set is owned by the deploying app.
///
/// An empty / IR-free directory is a clean no-op.
///
/// # Errors
/// [`SqliteIrApplyError`] on I/O / live-schema / fail-closed gate / apply failure.
// Eight cohesive deploy parameters (backend + identity + descriptor set + IR dir
// + exec/guard config + approval). This is the stable public SQLite-IR go-live
// entry with ~20 in-tree (test) call sites; bundling into a params struct would
// churn every caller for no readability gain and risks the behavior change this
// hygiene pass forbids. Each arg is a distinct concern.
#[allow(clippy::too_many_arguments)]
pub async fn apply_bundle_ir_sqlite(
    backend: &SqliteBackend,
    project_schema: &str,
    owner_app: &str,
    descriptors: &[CollectionDescriptor],
    migrations_dir: &Path,
    exec_cfg: &crate::ExecutorConfig,
    guard_cfg: &GuardConfig,
    approval: Approval,
) -> Result<SqliteIrApplyOutcome, SqliteIrApplyError> {
    // Discover `*.ir.json`, version-ordered by filename (deterministic).
    let mut ir_files: Vec<std::path::PathBuf> = Vec::new();
    let read = std::fs::read_dir(migrations_dir).map_err(|e| SqliteIrApplyError::Read {
        file: migrations_dir.display().to_string(),
        message: e.to_string(),
    })?;
    for entry in read {
        let entry = entry.map_err(|e| SqliteIrApplyError::Read {
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
        return Ok(SqliteIrApplyOutcome::default());
    }
    ir_files.sort();

    // Build the SQLite-dialect live facts (table_snapshots + sqlite_schemas +
    // unique_indexes + ownership) from the app's descriptor set. This is the SQLite
    // analogue of the PG path's live introspection — the SDK schema `Value`s the
    // rename rebuild needs come from the descriptor set, not a raw catalog read.
    //
    // DESCRIPTOR-SET CONTRACT (the SQLite live-facts model). Unlike the PG path,
    // which introspects the REAL catalog (`backend.snapshot_schema`) and so naturally
    // observes intermediate states, the SQLite leg cannot re-introspect the structural
    // rebuild facts (`table_snapshots`/`sqlite_schemas` — the SDK-`Value` facets a
    // `sqlite_master` read can't recover). The descriptor set plays TWO DISTINCT roles
    // here, and they have OPPOSITE state requirements — do not conflate them:
    //
    //   (A) OWNERSHIP / FK-INLINE REGISTRY — the union END-STATE. Every table any file
    //       in this directory creates or references is present in the descriptor set
    //       (the union of all member apps' registerModel schemas), so the ownership +
    //       inline-FK registry is complete from the start and needs NO per-file advance
    //       (unlike the PG path, which discovers tables per file from a live catalog).
    //
    //   (B) RENAME COLUMN FACTS — the PRE-rename LIVE shape. A SQLite `renameColumn`
    //       lowers to a 12-step REBUILD that reads the live `from` column off
    //       `table_snapshots`/`sqlite_schemas` to author the post-rename CREATE +
    //       value-copy. So for a rename, the descriptor set MUST carry the table's
    //       PRE-rename column facts (the `from` column must be PRESENT). A descriptor
    //       set derived from the app's registerModel = the POST-deploy DESIRED schema
    //       (post-rename: only `to` exists, `from` is gone) makes the rebuild author
    //       FAIL CLOSED (`RenameNeedsLiveColumn` / `SqliteRenameNeedsLiveTable`) — no
    //       data loss, but the rename is un-runnable from a post-rename descriptor set.
    //       The production SQLite wiring wave (see the module-level TODO) MUST source
    //       these PRE-rename column facts from a real pre-deploy SQLite-catalog/snapshot
    //       read (the SDK-Value facets reconstructed at deploy start), OR carry the
    //       pre-rename column in the IR — NOT from the post-deploy desired descriptor set.
    //
    // Consequently a multi-file deploy whose LATER file depends on an INTERMEDIATE
    // structural state produced by an EARLIER file in the SAME directory is NOT
    // supported: the rebuild author sees the (single, fixed) descriptor shape, not the
    // post-earlier-op shape, and FAILS CLOSED rather than emit a wrong rebuild. (Pinned
    // fail-closed by the e2e tests
    // `deploy_multi_file_intermediate_rename_fails_closed_on_real_sqlite` and
    // `deploy_post_rename_descriptor_set_fails_closed_on_real_sqlite`.)
    let live_schema =
        LiveSchema::for_sqlite_descriptors(project_schema, owner_app, descriptors)
            .map_err(SqliteIrApplyError::LiveSchema)?;
    // The ownership registry the IR-load gate enforces: every table in the app's
    // descriptor set (the END-STATE union of all member apps' schemas) is owned by the
    // deploying app. Because the descriptor set IS the union end-state, every table any
    // file in this directory creates or references is ALREADY present here — there is
    // nothing to advance per file (unlike the PG path, which starts from a live catalog
    // introspection and discovers per-file). A file that creates a table absent from
    // the descriptor set therefore has no owner here and fails closed at the IR-load
    // ownership gate — the descriptor set is the single authoritative ownership source.
    let registry: BTreeMap<String, String> = descriptors
        .iter()
        .map(|d| (d.name.clone(), owner_app.to_string()))
        .collect();

    run_ir_files_sqlite(
        backend,
        project_schema,
        owner_app,
        &registry,
        &live_schema,
        &ir_files,
        exec_cfg,
        guard_cfg,
        approval,
    )
    .await
}

/// **PR9b — the PRODUCTION SQLite IR-deploy entry (catalog-sourced live facts).**
/// The faithful peer of the PG `apply_bundle_ir_migrations`: it sources the rename
/// rebuild's PRE-rename column facts from a REAL pre-deploy SQLite-catalog read
/// (`LiveSchema::from_sqlite_catalog`), so a production caller — which naturally
/// derives `descriptors` from `registerModel` = the POST-deploy DESIRED schema (post
/// rename: only `to` exists) — can run a `renameColumn` as a rebuild WITHOUT a
/// hand-fed pre-rename descriptor set:
///
/// - the live `from` column (+ the whole live table shape) for the value-copy comes
///   from the live catalog (`table_snapshots`), NOT the descriptors;
/// - the post-rename SDK `Value` for the new-table CREATE comes from the `descriptors`
///   (the desired `to` field, with FULL facets — no lossy catalog reconstruction);
/// - the rebuild author pairs them (a rename preserves facets, so the desired `to`
///   facets ARE the live `from` facets).
///
/// The descriptors are still the ownership-registry source (role A — the END-STATE
/// union the IR-load gate checks). A rename whose live `from` column is genuinely
/// absent (a post-rename live DB / an intermediate state) still FAILS CLOSED in the
/// rebuild author — never a wrong rebuild.
///
/// Like [`apply_bundle_ir_sqlite`], a SQLite rename is an OFFLINE rebuild (no pending
/// contract); a rebuild on a populated table is destructive ⇒ `Approval::Approved`.
///
/// # Errors
/// [`SqliteIrApplyError`] on I/O / catalog-read / fail-closed gate / apply failure.
#[allow(clippy::too_many_arguments)]
pub async fn apply_bundle_ir_sqlite_catalog(
    backend: &SqliteBackend,
    project_schema: &str,
    owner_app: &str,
    descriptors: &[CollectionDescriptor],
    migrations_dir: &Path,
    exec_cfg: &crate::ExecutorConfig,
    guard_cfg: &GuardConfig,
    approval: Approval,
) -> Result<SqliteIrApplyOutcome, SqliteIrApplyError> {
    // Discover `*.ir.json`, version-ordered by filename (deterministic).
    let mut ir_files: Vec<std::path::PathBuf> = Vec::new();
    let read = std::fs::read_dir(migrations_dir).map_err(|e| SqliteIrApplyError::Read {
        file: migrations_dir.display().to_string(),
        message: e.to_string(),
    })?;
    for entry in read {
        let entry = entry.map_err(|e| SqliteIrApplyError::Read {
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
        return Ok(SqliteIrApplyOutcome::default());
    }
    ir_files.sort();

    // The PRE-rename live facts from a REAL catalog read (table_snapshots incl. `from`),
    // with the descriptor-sourced SDK Values (full facets). See
    // `LiveSchema::from_sqlite_catalog`.
    let live_schema = LiveSchema::from_sqlite_catalog(backend, owner_app, descriptors)
        .await
        .map_err(SqliteIrApplyError::Catalog)?;
    // Ownership registry: the descriptor END-STATE union (the IR-load gate's authority),
    // identical to the descriptor entry.
    let registry: BTreeMap<String, String> = descriptors
        .iter()
        .map(|d| (d.name.clone(), owner_app.to_string()))
        .collect();

    run_ir_files_sqlite(
        backend,
        project_schema,
        owner_app,
        &registry,
        &live_schema,
        &ir_files,
        exec_cfg,
        guard_cfg,
        approval,
    )
    .await
}

/// The shared per-file load+guard-lower+apply loop both SQLite IR-deploy entries use
/// once they have built their `LiveSchema` (descriptor-sourced or catalog-sourced) +
/// ownership registry. Factored out so the two entries differ ONLY in how the live
/// facts are sourced, not in the apply mechanics.
#[allow(clippy::too_many_arguments)]
async fn run_ir_files_sqlite(
    backend: &SqliteBackend,
    project_schema: &str,
    owner_app: &str,
    registry: &BTreeMap<String, String>,
    live_schema: &LiveSchema,
    ir_files: &[std::path::PathBuf],
    exec_cfg: &crate::ExecutorConfig,
    guard_cfg: &GuardConfig,
    approval: Approval,
) -> Result<SqliteIrApplyOutcome, SqliteIrApplyError> {
    let engine = MigrationEngine::new();
    let mut applied: Vec<String> = Vec::new();
    let mut skipped: Vec<String> = Vec::new();

    for path in ir_files {
        let file = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("<unknown>")
            .to_string();
        let bytes = std::fs::read_to_string(path).map_err(|e| SqliteIrApplyError::Read {
            file: file.clone(),
            message: e.to_string(),
        })?;

        let author = IrAuthor::new(project_schema, owner_app, SqlDialect::Sqlite);
        let lowered = author
            .load_and_lower_guarded(&bytes, owner_app, registry, live_schema, guard_cfg)
            .map_err(|source| SqliteIrApplyError::Ir { file: file.clone(), source })?;

        let outcome = engine
            .apply_plan(
                &lowered.plan.steps,
                approval,
                backend,
                exec_cfg,
                "deploy-ir-sqlite",
                LockMode::Acquire,
            )
            .await
            .map_err(SqliteIrApplyError::Apply)?;
        applied.extend(outcome.applied.applied);
        skipped.extend(outcome.applied.skipped);
    }

    Ok(SqliteIrApplyOutcome { applied, skipped })
}
