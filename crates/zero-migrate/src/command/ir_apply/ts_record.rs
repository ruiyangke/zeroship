//! The `zsv8`-gated `.ts`-record apply path.
//!
//! This submodule drives the V8 authoring front-end (`crate::frontend`): it
//! discovers `.ts` Platform migrations, records each to transient IR through
//! the sandboxed recorder, and feeds that IR through the SAME guarded lower +
//! apply core as committed-`.ir.json` bundle apply. It is compiled only under
//! the `zsv8` feature; the parent module's IR-load / sealed-apply surface is
//! V8-free.

use std::collections::BTreeMap;
use std::path::Path;

use crate::apply::backend::{MigrationBackend, PostgresBackend};
use crate::apply::executor::LockMode;
use crate::approval::ApprovalScope;
use crate::frontend::{record_migration_transient, DiscoveredMigration, RecordVia};
use crate::{
    Approval, DeclarativeApplyError, EngineError, ExecutorConfig, GuardConfig, PolicyProfile,
};

use super::{
    apply_one_ir_document_postgres, postgres_ir_apply_state, PlatformTsVersionSeed,
    PostgresIrApplyError, PostgresIrApplyOutcome,
};

/// Apply a Platform `.ts` migration corpus to Postgres by recording each source
/// file to transient IR at migrate time, then feeding that IR through the same
/// guarded lower + apply core as IR bundle apply.
///
/// This is Model C for trusted platform authors: `.ts` is the committed source of
/// truth, `.ir.json` is never written, and the existing sandboxed recorder is reused
/// as the TypeScript front-end. The project advisory lock is held once across the
/// corpus, matching [`super::apply_bundle_ir_postgres`].
#[allow(clippy::too_many_arguments)]
pub async fn apply_platform_ts_postgres(
    backend: &PostgresBackend<'_>,
    project_schema: &str,
    owner_app: &str,
    migrations_dir: &Path,
    record_via: &RecordVia<'_>,
    exec_cfg: &ExecutorConfig,
    guard_cfg: &GuardConfig,
    policy_profile: &PolicyProfile,
    approval: Approval,
    applied_by: &str,
) -> Result<PostgresIrApplyOutcome, PostgresIrApplyError> {
    let ts_migrations = crate::frontend::discover_migrations(migrations_dir).map_err(|source| {
        PostgresIrApplyError::Record {
            file: migrations_dir.display().to_string(),
            message: source.to_string(),
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
                    message: source.to_string(),
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
                policy_profile,
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
                    "zero-migrate: failed to release project lock after Platform TS apply"
                );
            }
            Err(e)
        }
    }
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
