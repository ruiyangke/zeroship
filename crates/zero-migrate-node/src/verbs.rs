//! The verb bodies behind the N-API entrypoints: the lock-bracketed engine
//! drivers and the engine-result to typed-reply projections.
//!
//! [`crate::bridge`] owns the Node ABI and nothing else: the `#[napi]`
//! entrypoints, the `ThreadsafeFunction` dispatch and the deferred promise.
//! Everything a verb decides after its arguments are decoded lives here, with no
//! napi type in any signature, so this module compiles and its tests run with the
//! `napi` feature off. That is the configuration the workspace gate builds, so
//! this logic is covered by tests that execute rather than only type-check.

use zero_migrate::apply::backend::{MigrationBackend, ProjectLockAcquisition, ProjectLockHolder};
use zero_migrate::apply::executor::{ApplyOutcome, LockMode, RollbackOptions, RollbackTarget};
use zero_migrate::approval::Approval;
use zero_migrate::conn::ExecutorConfig;
use zero_migrate::model::migration::Migration;
use zero_migrate::ops::status::{AppliedPlanStatus, MigrationStatus, PlanStatusManifest};
use zero_migrate::{LiveSchema, MigrationEngine, SqlDialect};

use crate::wire::{
    ApplyPendingContractDto, ApplyReply, BlockedPlanDto, PendingContractStatusDto, PlanStatusDto,
    PlanStatusStepDto, ProjectLockHolderDto, RollbackReply, StatusReply, UnexpectedJournalEntryDto,
};

/// The dialect a host-driven `apply` targets over the `SqlSession` seam. Only the
/// two NETWORK dialects reach the host driver: `SQLite` is in-process rusqlite and
/// never crosses the seam, so it is not a host-apply target.
#[derive(Debug, Clone, Copy)]
pub enum ApplyDialect {
    Postgres,
    Mysql,
}

impl ApplyDialect {
    /// Map the wire dialect spelling to the host-apply backend selector. `"sqlite"`
    /// is rejected here: it has no host-driver path (in-process rusqlite).
    pub fn parse(s: &str) -> std::result::Result<Self, String> {
        match s {
            "postgres" => Ok(Self::Postgres),
            "mysql" => Ok(Self::Mysql),
            "sqlite" => Err(
                "sqlite has no host-driver apply path (it runs in-process via rusqlite); \
                 pass a postgres or mysql driver"
                    .to_string(),
            ),
            other => Err(format!(
                "unknown dialect {other:?} (expected postgres|mysql for host apply)"
            )),
        }
    }
}

/// Borrow the ordered charter documents a request carries as the `&str` slice the
/// policy composer takes.
pub fn charter_layer_refs(charter_layers: &[String]) -> Vec<&str> {
    charter_layers.iter().map(String::as_str).collect()
}

/// Compose the ordered charter documents a request carries into the one effective
/// policy every verb runs under.
pub fn effective_policy_from_wire_layers(
    charter_layers: &[String],
) -> std::result::Result<zero_migrate::EffectivePolicy, String> {
    let layers = charter_layer_refs(charter_layers);
    zero_migrate::effective_policy_from_charter_layers(&layers)
}

/// Map the wire dialect spelling to the render dialect. Unlike
/// [`ApplyDialect::parse`] this accepts `"sqlite"`: an offline render needs no
/// host driver.
pub fn preview_dialect(s: &str) -> std::result::Result<SqlDialect, String> {
    match s {
        "postgres" => Ok(SqlDialect::Postgres),
        "sqlite" => Ok(SqlDialect::Sqlite),
        "mysql" => Ok(SqlDialect::Mysql),
        other => Err(format!(
            "unknown dialect {other:?} (expected postgres|sqlite|mysql)"
        )),
    }
}

/// Project an [`ApplyOutcome`] and the lock-coherent outstanding rename set into
/// the typed [`ApplyReply`].
pub fn apply_reply(
    outcome: ApplyOutcome,
    pending_contracts: &[zero_migrate::PendingContract],
) -> ApplyReply {
    ApplyReply {
        applied: outcome.applied,
        skipped: outcome.skipped,
        recovered: outcome.recovered,
        pending_contracts: pending_contracts
            .iter()
            .map(|contract| ApplyPendingContractDto {
                table: contract.table.clone(),
                from_column: contract.from_col.clone(),
                to_column: contract.to_col.clone(),
                pending_version: contract.pending_version.clone(),
            })
            .collect(),
    }
}

/// The reply a status verb returns when a peer's deploy holds the project lock.
///
/// Every reconciled field is empty because NO catalog or journal read ran: the
/// reads are composite and unbracketed, and a non-transactional apply commits its
/// inflight marker before the DDL and its completed row after, so a reader that
/// went ahead without the lock would report a live deploy's halfway state as drift
/// and fail a strict gate that has nothing wrong with it. `busy` is what callers
/// branch on; the holders are what the operator message names.
fn project_lock_busy_reply(holders: &[ProjectLockHolder]) -> StatusReply {
    StatusReply {
        current_version: None,
        applied: Vec::new(),
        pending: Vec::new(),
        aborted: Vec::new(),
        rolled_back: Vec::new(),
        pending_contracts: Vec::new(),
        blocked: Vec::new(),
        unexpected_journal: Vec::new(),
        plans: None,
        busy: true,
        lock_holders: holders
            .iter()
            .map(|holder| ProjectLockHolderDto {
                pid: holder.pid,
                application_name: holder.application_name.clone(),
                state: holder.state.clone(),
                query: holder.query.clone(),
            })
            .collect(),
    }
}

/// Project a [`MigrationStatus`] into the typed [`StatusReply`] (the load-bearing
/// fields: current version + applied/pending/rolled-back version ids).
pub fn status_reply(s: &MigrationStatus) -> StatusReply {
    StatusReply {
        current_version: s.current_version.as_ref().map(|v| v.as_str().to_string()),
        applied: s.applied.iter().map(|e| e.version.clone()).collect(),
        pending: s.pending.iter().map(|v| v.as_str().to_string()).collect(),
        aborted: Vec::new(),
        rolled_back: s.rolled_back.iter().map(|e| e.version.clone()).collect(),
        pending_contracts: s
            .pending_contracts
            .iter()
            .map(|contract| PendingContractStatusDto {
                table: contract.table.clone(),
                pending_version: contract.pending_version.clone(),
                orphaned: contract.orphaned,
            })
            .collect(),
        blocked: s
            .blocked
            .iter()
            .map(|blocked| BlockedPlanDto {
                blocked: blocked.blocked.as_str().to_string(),
                dependency: blocked.dependency.as_str().to_string(),
                pending_version: blocked.pending_version.clone(),
            })
            .collect(),
        unexpected_journal: Vec::new(),
        plans: None,
        busy: false,
        lock_holders: Vec::new(),
    }
}

/// Project a complete-plan reconciliation into the shared status reply shape.
/// Top-level ids are LOGICAL PLAN ids; `plans[].steps` carries the actual journal
/// identities and their individual states.
pub fn plan_status_reply(status: &AppliedPlanStatus) -> StatusReply {
    let plans = status
        .plans
        .iter()
        .map(|plan| PlanStatusDto {
            version: plan.version.as_str().to_string(),
            name: plan.name.clone(),
            state: plan.state.as_str().to_string(),
            steps: plan
                .steps
                .iter()
                .map(|step| PlanStatusStepDto {
                    version: step.version.as_str().to_string(),
                    name: step.name.clone(),
                    kind: step.kind.as_str().to_string(),
                    state: step.state.as_str().to_string(),
                    cursor_stability_mode: step.cursor_stability_mode.clone(),
                    cursor_stability_invariant: step.cursor_stability_invariant.clone(),
                    writes_quiesced: step.writes_quiesced.clone(),
                })
                .collect(),
            missing_dependencies: plan
                .missing_dependencies
                .iter()
                .map(|dependency| dependency.as_str().to_string())
                .collect(),
        })
        .collect();
    StatusReply {
        current_version: status
            .current_version
            .as_ref()
            .map(|version| version.as_str().to_string()),
        applied: status
            .applied
            .iter()
            .map(|version| version.as_str().to_string())
            .collect(),
        pending: status
            .pending
            .iter()
            .map(|version| version.as_str().to_string())
            .collect(),
        aborted: status
            .aborted
            .iter()
            .map(|version| version.as_str().to_string())
            .collect(),
        rolled_back: status.rolled_back.clone(),
        pending_contracts: status
            .pending_contracts
            .iter()
            .map(|contract| PendingContractStatusDto {
                table: contract.table.clone(),
                pending_version: contract.pending_version.clone(),
                orphaned: contract.orphaned,
            })
            .collect(),
        blocked: status
            .blocked
            .iter()
            .map(|blocked| BlockedPlanDto {
                blocked: blocked.blocked.as_str().to_string(),
                dependency: blocked.dependency.as_str().to_string(),
                pending_version: blocked.pending_version.clone(),
            })
            .collect(),
        unexpected_journal: status
            .unexpected_journal
            .iter()
            .map(|entry| UnexpectedJournalEntryDto {
                version: entry.version.clone(),
                state: entry.state.as_str().to_string(),
                journal_checksum: entry.journal_checksum.clone(),
                journal_kind: entry.journal_kind.map(|kind| kind.as_str().to_string()),
            })
            .collect(),
        plans: Some(plans),
        busy: false,
        lock_holders: Vec::new(),
    }
}

/// Snapshot, lower, and apply one authored envelope inside one project-lock
/// bracket. The catalog facts used by lowering must describe the same serialized
/// database state that the executor mutates; taking the snapshot before the lock
/// would leave a check-then-use window for a concurrent deploy.
pub async fn apply_ir_with_locked_backend<B: MigrationBackend>(
    backend: &B,
    cfg: &ExecutorConfig,
    prior_envelope_json: &[String],
    envelope_json: &str,
    owner_app: &str,
    project_schema: &str,
    dialect: &str,
    registry_json: &str,
    charter_layers: &[String],
    approval: Approval,
    applied_by: &str,
) -> std::result::Result<ApplyReply, String> {
    let charter_refs = charter_layer_refs(charter_layers);
    backend
        .ensure_journal(cfg)
        .await
        .map_err(|error| error.to_string())?;
    backend
        .acquire_project_lock(cfg)
        .await
        .map_err(|error| format!("failed to acquire project lock: {error}"))?;

    let result = async {
        let snapshot = backend
            .snapshot_schema(cfg)
            .await
            .map_err(|error| format!("live schema introspection failed: {error}"))?;
        let journal_entries = backend
            .applied(cfg)
            .await
            .map_err(|error| error.to_string())?;
        let resolved_contracts = match backend.pending_contracts() {
            Some(capability) => capability
                .resolved_pending_contracts(cfg)
                .await
                .map_err(|error| error.to_string())?,
            None => Vec::new(),
        };
        let live = LiveSchema::from_catalog_snapshot(snapshot.clone(), owner_app);
        // No priors means the caller declared NO authored prefix, not that this is the
        // operator's first migration -- the library `apply()` surface leaves them out
        // on every call. So there is nothing to reconcile the journal against here,
        // and a completed step this lone envelope does not own says nothing.
        let artifact = if prior_envelope_json.is_empty() {
            match crate::lower::lower_envelope_to_plan_with_live(
                envelope_json,
                owner_app,
                project_schema,
                dialect,
                registry_json,
                &charter_refs,
                &live,
            ) {
                Ok(artifact) => artifact,
                Err(_) => {
                    let mut artifacts = crate::lower::lower_ordered_envelopes_to_plans_for_apply(
                        &[envelope_json.to_string()],
                        owner_app,
                        project_schema,
                        dialect,
                        registry_json,
                        &charter_refs,
                        snapshot,
                        &journal_entries,
                        &resolved_contracts,
                    )?;
                    artifacts.pop().ok_or_else(|| {
                        "lowering returned no plan for the migration envelope".to_string()
                    })?
                }
            }
        } else {
            let mut ordered_envelopes = prior_envelope_json.to_vec();
            ordered_envelopes.push(envelope_json.to_string());
            let mut artifacts = crate::lower::lower_ordered_envelopes_to_plans_for_apply(
                &ordered_envelopes,
                owner_app,
                project_schema,
                dialect,
                registry_json,
                &charter_refs,
                snapshot,
                &journal_entries,
                &resolved_contracts,
            )?;
            let manifests = artifacts
                .iter()
                .map(|artifact| {
                    PlanStatusManifest::from_applied_plan(&artifact.plan, &artifact.depends_on)
                        .map_err(|error| error.to_string())
                })
                .collect::<std::result::Result<Vec<_>, _>>()?;
            let status = zero_migrate::ops::status::status_plans_via_backend_locked(
                backend, cfg, &manifests,
            )
            .await
            .map_err(|error| error.to_string())?;
            // The journal rides along: `event_seq` is what tells a migration the
            // operator deleted from one this per-file call was not handed yet.
            crate::lower::require_applied_prefix(
                &manifests,
                prior_envelope_json.len(),
                &status,
                &journal_entries,
            )?;
            artifacts.pop().ok_or_else(|| {
                "lowering returned no plan for the current migration envelope".to_string()
            })?
        };
        let outcome = MigrationEngine::new()
            .apply_applied_plan_with_touched_and_depends(
                &artifact.plan,
                &artifact.touched_tables,
                &artifact.depends_on,
                approval,
                backend,
                cfg,
                applied_by,
                LockMode::AlreadyHeld,
            )
            .await
            .map_err(|error| error.to_string())?;
        let pending_contracts = match backend.pending_contracts() {
            Some(capability) => capability
                .outstanding_pending_contracts(cfg)
                .await
                .map_err(|error| error.to_string())?,
            None => Vec::new(),
        };
        Ok::<ApplyReply, String>(apply_reply(outcome.applied, &pending_contracts))
    }
    .await;

    let release = backend.release_project_lock(cfg).await;
    match (result, release) {
        (Ok(reply), Ok(())) => Ok(reply),
        (Ok(_), Err(error)) => Err(format!("failed to release project lock: {error}")),
        (Err(error), Ok(())) => Err(error),
        (Err(error), Err(release_error)) => Err(format!(
            "{error}; additionally failed to release project lock: {release_error}"
        )),
    }
}

/// Decode the wire spelling of how far a rollback should unwind.
///
/// The three shapes are mutually exclusive and each carries its own operand, so an
/// operand set for the wrong kind is REFUSED rather than dropped. An operator who
/// asked to unwind two steps and also typed a version has made a mistake worth
/// hearing about before anything comes down, not after.
pub fn parse_rollback_target(
    kind: &str,
    version: Option<&str>,
    steps: Option<u32>,
) -> std::result::Result<RollbackTarget, String> {
    let reject_extra = |field: &str, present: bool| {
        if present {
            Err(format!(
                "rollback target {kind:?} does not take a {field}; drop it or change the target kind"
            ))
        } else {
            Ok(())
        }
    };
    match kind {
        "toVersion" => {
            reject_extra("step count", steps.is_some())?;
            let version = version.ok_or_else(|| {
                "rollback target \"toVersion\" needs the version to unwind down to".to_string()
            })?;
            let id = zero_migrate::model::migration::MigrationId::parse(version)
                .map_err(|error| format!("invalid rollback target version: {error}"))?;
            Ok(RollbackTarget::ToVersion(id))
        }
        "steps" => {
            reject_extra("version", version.is_some())?;
            let steps = steps.ok_or_else(|| {
                "rollback target \"steps\" needs how many migrations to unwind".to_string()
            })?;
            Ok(RollbackTarget::Steps(steps as usize))
        }
        "all" => {
            reject_extra("version", version.is_some())?;
            reject_extra("step count", steps.is_some())?;
            Ok(RollbackTarget::All)
        }
        other => Err(format!(
            "unknown rollback target {other:?} (expected toVersion|steps|all)"
        )),
    }
}

/// The authored set a rollback reverses, plus the journaled identities it could
/// not represent.
struct RollbackSet {
    /// One `Migration` per artifact that lowered to exactly ONE DDL step.
    migrations: Vec<Migration>,
    /// Journaled step identity to the authored migration that owns it, for every
    /// step of a plan that lowered to more than one. These are the identities the
    /// engine refuses as `MissingFromSet`, and the map is what turns that refusal
    /// from an opaque derived version into a name the operator authored.
    unrepresentable: std::collections::BTreeMap<String, String>,
}

/// Project the lowered artifacts into the authored migration set `rollback` takes.
///
/// The engine reverses `Migration`s, and a `Migration` is the journaled identity of
/// exactly one DDL step. A plan that lowers to several steps journals a separate
/// identity per step, and its DML, backfill, identity-synchronisation and
/// online-rename steps carry no `down` at all. Handing the engine only the DDL
/// steps of such a plan would present it as fully reversible while silently
/// dropping the steps a rollback must refuse to cross.
///
/// So a multi-step plan contributes NOTHING here. That is safe rather than lax:
/// the planner refuses any selected version absent from the supplied set with
/// `MissingFromSet` before a single `down` runs, so the omission ends the rollback
/// instead of hiding inside it.
fn rollback_migration_set(
    artifacts: &[zero_migrate::LoweredArtifact],
) -> std::result::Result<RollbackSet, String> {
    let mut set = RollbackSet {
        migrations: Vec::with_capacity(artifacts.len()),
        unrepresentable: std::collections::BTreeMap::new(),
    };
    for artifact in artifacts {
        if let Ok(migration) = artifact.plan.single_step_migration() {
            set.migrations.push(migration.clone());
            continue;
        }
        // The manifest is the one walker that already enumerates every step
        // variant's journal identity, so the refusal can name the step kind
        // without teaching this module the shape of each variant.
        let manifest = PlanStatusManifest::from_applied_plan(&artifact.plan, &artifact.depends_on)
            .map_err(|error| error.to_string())?;
        for step in &manifest.steps {
            set.unrepresentable.insert(
                step.version.as_str().to_string(),
                format!("{} ({} step)", manifest.name, step.kind.as_str()),
            );
        }
    }
    Ok(set)
}

/// Unwind applied migrations inside one project-lock bracket.
///
/// The lock is taken WITH waiting, unlike `status`: a rollback writes, so it is a
/// peer of the deploy it is undoing rather than a reader that can decline and
/// report busy. Giving up on contention would leave the operator to retry an
/// unwind by hand while the schema sits half-migrated.
///
/// The live catalog is read after the lock for the same reason `apply` reads it
/// there: lowering the authored envelopes against a snapshot taken before the lock
/// would reconstruct the `down` SQL from state a concurrent deploy has since moved.
pub async fn rollback_with_locked_backend<B: MigrationBackend>(
    backend: &B,
    cfg: &ExecutorConfig,
    envelope_json: &[String],
    owner_app: &str,
    project_schema: &str,
    dialect: &str,
    registry_json: &str,
    charter_layers: &[String],
    target: RollbackTarget,
    options: RollbackOptions,
    approval: Approval,
    applied_by: &str,
) -> std::result::Result<RollbackReply, String> {
    let charter_refs = charter_layer_refs(charter_layers);
    backend
        .ensure_journal(cfg)
        .await
        .map_err(|error| error.to_string())?;
    backend
        .acquire_project_lock(cfg)
        .await
        .map_err(|error| format!("failed to acquire project lock: {error}"))?;

    let result = async {
        let snapshot = backend
            .snapshot_schema(cfg)
            .await
            .map_err(|error| format!("live schema introspection failed: {error}"))?;
        let journal_entries = backend
            .applied(cfg)
            .await
            .map_err(|error| error.to_string())?;
        let resolved_contracts = match backend.pending_contracts() {
            Some(capability) => capability
                .resolved_pending_contracts(cfg)
                .await
                .map_err(|error| error.to_string())?,
            None => Vec::new(),
        };
        // Rollback lowers against what the history LEFT BEHIND, not what apply is moving
        // towards: the inverse of a drop needs the definition the drop removed, and the
        // pending projection cannot see it once everything is applied.
        let artifacts = crate::lower::lower_ordered_envelopes_to_plans_for_rollback(
            envelope_json,
            owner_app,
            project_schema,
            dialect,
            registry_json,
            &charter_refs,
            snapshot,
            &journal_entries,
        )?;
        let set = rollback_migration_set(&artifacts)?;
        let request = zero_migrate::RollbackRequest::new(target).with_options(options);
        // The guard the engine's own apply sites use. Composing one from the same
        // charter here would drop the config's host-selected mode.
        let guard = zero_migrate::guard_for(&cfg.guard_config().for_dialect(backend.dialect()));
        let outcome = zero_migrate::rollback_with_lock(
            backend,
            cfg,
            &request,
            &set.migrations,
            approval,
            applied_by,
            guard.as_ref(),
            LockMode::AlreadyHeld,
        )
        .await
        .map_err(|error| describe_rollback_error(&error, &set))?;
        Ok::<RollbackReply, String>(RollbackReply {
            rolled_back: outcome.rolled_back,
            skipped_irreversible: outcome.skipped_irreversible,
        })
    }
    .await;

    let release = backend.release_project_lock(cfg).await;
    match (result, release) {
        (Ok(reply), Ok(())) => Ok(reply),
        (Ok(_), Err(error)) => Err(format!("failed to release project lock: {error}")),
        (Err(error), Ok(())) => Err(error),
        (Err(error), Err(release_error)) => Err(format!(
            "{error}; additionally failed to release project lock: {release_error}"
        )),
    }
}

/// Name the authored migration behind a refusal the engine can only report as a
/// derived version, so the operator is told which file to look at.
fn describe_rollback_error(error: &zero_migrate::RollbackError, set: &RollbackSet) -> String {
    let zero_migrate::RollbackError::MissingFromSet { version } = error else {
        return error.to_string();
    };
    set.unrepresentable.get(version.as_str()).map_or_else(
        || error.to_string(),
        |owner| {
            format!(
                "{error}. That version is {owner}, and a plan that lowers to more than \
                 one journaled step cannot be reversed from its authored envelope: its \
                 data steps carry no reverse SQL. Roll forward with a compensating \
                 migration instead"
            )
        },
    )
}

/// Lower and reconcile authored plans while holding the same project lock across
/// the live-catalog and journal reads.
///
/// The lock is taken WITHOUT waiting. `status` is the documented CI gate and
/// `plan` is the read-only preview, and a deploy holds this lock for its whole
/// run, so waiting would put both behind an unbounded stall every time a peer
/// deploys. A contended acquisition returns the busy reply instead, having read
/// nothing.
pub async fn status_ir_with_locked_backend<B: MigrationBackend>(
    backend: &B,
    cfg: &ExecutorConfig,
    envelope_json: &[String],
    owner_app: &str,
    project_schema: &str,
    dialect: &str,
    registry_json: &str,
    charter_layers: &[String],
    read_only: bool,
) -> std::result::Result<StatusReply, String> {
    let charter_refs = charter_layer_refs(charter_layers);
    if !read_only {
        backend
            .ensure_journal(cfg)
            .await
            .map_err(|error| error.to_string())?;
    }
    match backend
        .try_acquire_project_lock(cfg)
        .await
        .map_err(|error| format!("failed to acquire project lock: {error}"))?
    {
        ProjectLockAcquisition::Acquired => {}
        // Nothing was locked, so there is nothing to release and nothing to read.
        ProjectLockAcquisition::Busy(holders) => return Ok(project_lock_busy_reply(&holders)),
    }

    let result = async {
        let snapshot = backend
            .snapshot_schema(cfg)
            .await
            .map_err(|error| format!("live schema introspection failed: {error}"))?;
        let journal_exists = if read_only {
            backend
                .journal_exists(cfg)
                .await
                .map_err(|error| error.to_string())?
        } else {
            true
        };
        let journal_entries = if journal_exists {
            backend
                .applied(cfg)
                .await
                .map_err(|error| error.to_string())?
        } else {
            Vec::new()
        };
        let resolved_contracts = if journal_exists {
            match backend.pending_contracts() {
                Some(capability) => capability
                    .resolved_pending_contracts(cfg)
                    .await
                    .map_err(|error| error.to_string())?,
                None => Vec::new(),
            }
        } else {
            Vec::new()
        };
        let artifacts = crate::lower::lower_ordered_envelopes_to_plans(
            envelope_json,
            owner_app,
            project_schema,
            dialect,
            registry_json,
            &charter_refs,
            snapshot,
            &journal_entries,
            &resolved_contracts,
        )?;
        let manifests = artifacts
            .iter()
            .map(|artifact| {
                PlanStatusManifest::from_applied_plan(&artifact.plan, &artifact.depends_on)
                    .map_err(|error| error.to_string())
            })
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let status = if read_only {
            zero_migrate::ops::status::status_plans_via_backend_read_only_locked(
                backend, cfg, &manifests,
            )
            .await
        } else {
            zero_migrate::ops::status::status_plans_via_backend_locked(backend, cfg, &manifests)
                .await
        }
        .map_err(|error| error.to_string())?;
        Ok::<StatusReply, String>(plan_status_reply(&status))
    }
    .await;

    let release = backend.release_project_lock(cfg).await;
    match (result, release) {
        (Ok(reply), Ok(())) => Ok(reply),
        (Ok(_), Err(error)) => Err(format!("failed to release project lock: {error}")),
        (Err(error), Ok(())) => Err(error),
        (Err(error), Err(release_error)) => Err(format!(
            "{error}; additionally failed to release project lock: {release_error}"
        )),
    }
}

/// Read migration-only status through the selected dialect backend while holding
/// one project lock across the journal buckets. The lock is taken without waiting,
/// for the same reason as the plan-aware verb: a reader must not inherit the wall
/// clock of a peer's deploy. The core legacy status carrier
/// retains detailed PostgreSQL rollback rows, while the neutral backend trait
/// exposes rollback version ids; the Node reply needs only those ids, so project
/// them directly without sending PostgreSQL-only SQL to MySQL.
pub async fn legacy_status_with_locked_backend<B: MigrationBackend>(
    backend: &B,
    cfg: &ExecutorConfig,
    migrations: &[Migration],
) -> std::result::Result<StatusReply, String> {
    backend
        .ensure_journal(cfg)
        .await
        .map_err(|error| error.to_string())?;
    match backend
        .try_acquire_project_lock(cfg)
        .await
        .map_err(|error| format!("failed to acquire project lock: {error}"))?
    {
        ProjectLockAcquisition::Acquired => {}
        // Nothing was locked, so there is nothing to release and nothing to read.
        ProjectLockAcquisition::Busy(holders) => return Ok(project_lock_busy_reply(&holders)),
    }

    let result = async {
        let status = zero_migrate::ops::status::status_via_backend_locked(backend, cfg, migrations)
            .await
            .map_err(|error| error.to_string())?;
        let rolled_back = backend
            .net_rolled_back_versions(cfg)
            .await
            .map_err(|error| error.to_string())?;
        let mut reply = status_reply(&status);
        reply.rolled_back = rolled_back;
        Ok::<StatusReply, String>(reply)
    }
    .await;

    let release = backend.release_project_lock(cfg).await;
    match (result, release) {
        (Ok(reply), Ok(())) => Ok(reply),
        (Ok(_), Err(error)) => Err(format!("failed to release project lock: {error}")),
        (Err(error), Ok(())) => Err(error),
        (Err(error), Err(release_error)) => Err(format!(
            "{error}; additionally failed to release project lock: {release_error}"
        )),
    }
}

/// Resolve one durable PostgreSQL online-rename obligation and return the
/// remaining obligations from the same project-lock bracket.
pub async fn resolve_pending_with_locked_backend<B: MigrationBackend>(
    backend: &B,
    cfg: &ExecutorConfig,
    pending_version: &str,
    resolution: zero_migrate::Resolution,
    owner_app: &str,
    approval: Approval,
    applied_by: &str,
) -> std::result::Result<ApplyReply, String> {
    // Keep the approval failure DB-free. The engine enforces this again as a
    // defense in depth, but this adapter owns the outer lock bracket.
    if approval != Approval::Approved {
        return Err("explicit approval is required to resolve a pending contract".to_string());
    }

    backend
        .acquire_project_lock(cfg)
        .await
        .map_err(|error| format!("failed to acquire project lock: {error}"))?;

    let result = async {
        let outcome = MigrationEngine::new()
            .resolve_pending_contract_with_lock(
                pending_version,
                resolution,
                owner_app,
                approval,
                backend,
                cfg,
                applied_by,
                LockMode::AlreadyHeld,
            )
            .await
            .map_err(|error| error.to_string())?;
        let pending = backend
            .pending_contracts()
            .ok_or_else(|| "this backend does not support pending contracts".to_string())?
            .outstanding_pending_contracts(cfg)
            .await
            .map_err(|error| error.to_string())?;
        Ok::<ApplyReply, String>(apply_reply(outcome.applied, &pending))
    }
    .await;

    let release = backend.release_project_lock(cfg).await;
    match (result, release) {
        (Ok(reply), Ok(())) => Ok(reply),
        (Ok(_), Err(error)) => Err(format!("failed to release project lock: {error}")),
        (Err(error), Ok(())) => Err(error),
        (Err(error), Err(release_error)) => Err(format!(
            "{error}; additionally failed to release project lock: {release_error}"
        )),
    }
}

/// The `project_id` an `ExecutorConfig` carries. The IR host path uses the project
/// schema as the project id (a fresh single-app project's schema == its id in the
/// create-first posture). A distinct project id can be threaded through a future
/// facade arg.
pub fn owner_app_project(project_schema: &str) -> String {
    project_schema.to_string()
}

#[cfg(test)]
mod status_projection_tests {
    use super::*;
    use zero_migrate::apply::journal::JournaledKind;
    use zero_migrate::model::migration::MigrationId;
    use zero_migrate::ops::status::{BlockedPlan, PendingContractStatus, UnexpectedJournalEntry};

    #[test]
    fn plan_status_reply_preserves_operator_details() {
        let blocked_version = MigrationId::derive("node_status", b"blocked");
        let dependency = MigrationId::derive("node_status", b"dependency");
        let aborted_version = MigrationId::derive("node_status", b"aborted");
        let status = AppliedPlanStatus {
            current_version: None,
            applied: Vec::new(),
            pending: vec![blocked_version.clone()],
            aborted: vec![aborted_version.clone()],
            rolled_back: vec!["mig_rolled_back".to_string()],
            plans: Vec::new(),
            unexpected_journal: vec![UnexpectedJournalEntry {
                version: "mig_unexpected".to_string(),
                state: zero_migrate::ops::status::PlanStatusStepState::Applied,
                journal_checksum: "checksum".to_string(),
                journal_kind: Some(JournaledKind::Apply),
            }],
            pending_contracts: vec![PendingContractStatus {
                table: "widgets".to_string(),
                pending_version: "mig_pending_contract".to_string(),
                orphaned: false,
            }],
            blocked: vec![BlockedPlan {
                blocked: blocked_version.clone(),
                dependency: dependency.clone(),
                pending_version: "mig_pending_contract".to_string(),
            }],
        };

        let reply = plan_status_reply(&status);

        assert_eq!(reply.aborted, vec![aborted_version.as_str()]);

        assert_eq!(reply.rolled_back, ["mig_rolled_back"]);
        assert_eq!(reply.pending_contracts[0].table, "widgets");
        assert_eq!(reply.blocked[0].blocked, blocked_version.as_str());
        assert_eq!(reply.blocked[0].dependency, dependency.as_str());
        assert_eq!(reply.unexpected_journal[0].state, "applied");
        assert_eq!(
            reply.unexpected_journal[0].journal_kind.as_deref(),
            Some("apply")
        );
    }

    #[test]
    fn sqlite_is_not_a_host_apply_dialect() {
        assert!(matches!(
            ApplyDialect::parse("postgres"),
            Ok(ApplyDialect::Postgres)
        ));
        assert!(matches!(
            ApplyDialect::parse("mysql"),
            Ok(ApplyDialect::Mysql)
        ));
        // SQLite runs in-process, so routing it at a host driver would deadlock on a
        // seam no driver answers: it must be rejected, and named in the message.
        let sqlite = ApplyDialect::parse("sqlite").expect_err("sqlite has no host-driver path");
        assert!(sqlite.contains("rusqlite"), "{sqlite}");
        let unknown = ApplyDialect::parse("Postgres").expect_err("the spelling is exact");
        assert!(unknown.contains("unknown dialect"), "{unknown}");
        // The offline renderer has no host driver to route at, so it takes sqlite.
        assert_eq!(preview_dialect("sqlite"), Ok(SqlDialect::Sqlite));
        assert!(preview_dialect("oracle").is_err());
    }

    #[test]
    fn every_rollback_target_shape_decodes_and_carries_only_its_own_operand() {
        assert_eq!(
            parse_rollback_target("all", None, None),
            Ok(RollbackTarget::All)
        );
        assert_eq!(
            parse_rollback_target("steps", None, Some(2)),
            Ok(RollbackTarget::Steps(2))
        );
        let version = zero_migrate::model::migration::MigrationId::generate();
        assert_eq!(
            parse_rollback_target("toVersion", Some(version.as_str()), None),
            Ok(RollbackTarget::ToVersion(version.clone()))
        );

        // A missing operand is a question, not a default: unwinding "some" of a
        // schema has no safe fallback, so each kind demands its own.
        let no_version = parse_rollback_target("toVersion", None, None)
            .expect_err("toVersion has nothing to stop at");
        assert!(no_version.contains("unwind down to"), "{no_version}");
        let no_count =
            parse_rollback_target("steps", None, None).expect_err("steps has nothing to count");
        assert!(no_count.contains("how many"), "{no_count}");

        // An operand belonging to a different kind is REFUSED rather than dropped:
        // silently ignoring it would tear down more than the operator described.
        for (kind, version, steps) in [
            ("all", Some(version.as_str()), None),
            ("all", None, Some(3)),
            ("steps", Some(version.as_str()), Some(3)),
            ("toVersion", Some(version.as_str()), Some(3)),
        ] {
            let error = parse_rollback_target(kind, version, steps)
                .expect_err("an operand for another kind is a mistake, not noise");
            assert!(error.contains("does not take"), "{kind}: {error}");
        }

        let unknown =
            parse_rollback_target("everything", None, None).expect_err("the spelling is exact");
        assert!(unknown.contains("unknown rollback target"), "{unknown}");
        let malformed = parse_rollback_target("toVersion", Some("not-a-version"), None)
            .expect_err("a version that is not one cannot select anything");
        assert!(
            malformed.contains("invalid rollback target version"),
            "{malformed}"
        );
    }
}
