//! The verb bodies behind the N-API entrypoints: the lock-bracketed engine
//! drivers and the engine-result to typed-reply projections.
//!
//! [`crate::bridge`] owns the Node ABI and nothing else: the `#[napi]`
//! entrypoints, the `ThreadsafeFunction` dispatch and the deferred promise.
//! Everything a verb decides after its arguments are decoded lives here, with no
//! napi type in any signature, so this module compiles and its tests run with the
//! `napi` feature off. That is the configuration the workspace gate builds, so
//! this logic is covered by tests that execute rather than only type-check.

use zeroship_migrate::apply::backend::{MigrationBackend, ProjectLockAcquisition, ProjectLockHolder};
use zeroship_migrate::apply::executor::{ApplyOutcome, LockMode, RollbackOptions, RollbackTarget};
use zeroship_migrate::approval::Approval;
use zeroship_migrate::conn::ExecutorConfig;
use zeroship_migrate::model::migration::Migration;
use zeroship_migrate::ops::status::{AppliedPlanStatus, MigrationStatus, PlanStatusManifest};
use zeroship_migrate::{shipping_backends, DialectId, LiveSchema, MigrationEngine};
use zeroship_migrate_mysql::DIALECT as MYSQL;
use zeroship_migrate_postgres::DIALECT as POSTGRES;
use zeroship_migrate_sqlite::DIALECT as SQLITE;

use crate::wire::{
    ApplyPendingContractDto, ApplyReply, BaselineReply, BaselineStepDto, BlockedPlanDto,
    PendingContractStatusDto, PlanStatusDto, PlanStatusStepDto, ProjectLockHolderDto,
    RollbackReply, StatusReply, UnexpectedJournalEntryDto,
};

/// The dialect a host-driven verb targets over the `SqlSession` seam. Only the
/// two NETWORK dialects reach the host driver: `SQLite` is in-process rusqlite and
/// never crosses the seam, so it is not a host-driver target.
///
/// Each variant CARRIES its [`DialectId`] rather than being one. The wire
/// spelling a host sends is matched against registered backend ids, not against
/// string literals, so `"postgres"` cannot mean one thing here and another in
/// the dialect table. The variants stay because `bridge.rs` selects a concrete
/// backend type per arm and that dispatch must remain exhaustive until the
/// backend crates exist to be dispatched to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApplyDialect {
    Postgres,
    Mysql,
}

impl ApplyDialect {
    /// The id this target denotes.
    #[must_use]
    pub const fn id(self) -> DialectId {
        match self {
            Self::Postgres => POSTGRES,
            Self::Mysql => MYSQL,
        }
    }

    /// Map the wire dialect spelling to the host-driver backend selector.
    ///
    /// A spelling must first name a REGISTERED backend; only then is it asked
    /// whether it has a host-driver path. `"sqlite"` names a registered backend
    /// and is still refused, because it runs in-process via rusqlite - that is a
    /// posture, not an unknown dialect, and the two get different diagnostics.
    pub fn parse(s: &str) -> std::result::Result<Self, String> {
        let registry = shipping_backends();
        let descriptor = registry
            .iter()
            .find(|descriptor| descriptor.id.as_str() == s)
            .ok_or_else(|| {
                format!("unknown dialect {s:?} (expected postgres|mysql over a host driver)")
            })?;

        for target in [Self::Postgres, Self::Mysql] {
            if target.id() == descriptor.id {
                return Ok(target);
            }
        }

        Err(format!(
            "{} has no host-driver path (it runs in-process via rusqlite); pass an \
             {IN_PROCESS_DRIVER_KIND:?} driver for it, or a postgres or mysql dialect over \
             this one",
            descriptor.id
        ))
    }
}

/// The `driver.kind` spelling for a verb the JS caller drives.
pub const HOST_DRIVER_KIND: &str = "host";
/// The `driver.kind` spelling for a verb the addon drives itself.
pub const IN_PROCESS_DRIVER_KIND: &str = "inProcess";

/// The journal credentials the HOST arm of one verb's driver carries.
///
/// Who opens the connection is the same question for `applyIr`, `statusIr` and
/// `rollback`, and [`DriverTarget::resolve`] answers it once for all three. What the
/// verb WRITES is not the same question, and this is the one axis where they differ:
/// a status records no journal row a caller labels, a rollback records one under a
/// label BOTH of its drivers read off the request, and an apply records one only its
/// host driver supplies, because its in-process driver hands the sequence to
/// [`MigrationEngine::deploy_envelopes`], which takes no label at all.
///
/// Stating that as a type per verb rather than as a flag keeps each verb's call site
/// reading exactly the fields its driver carries: there is no `Option` for a caller
/// to unwrap on a rule the resolution already enforced.
///
/// [`MigrationEngine::deploy_envelopes`]: zeroship_migrate::MigrationEngine::deploy_envelopes
pub trait HostCredentials: Sized {
    /// Read this verb's credential fields off a host driver, or refuse a field the
    /// verb's driver does not carry.
    ///
    /// # Errors
    /// Returns the refusal naming the offending field and where it belongs.
    fn decode(parts: &DriverParts<'_>) -> std::result::Result<Self, String>;
}

/// A verb whose driver carries no journal credentials at all: `statusIr`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NoCredentials;

impl HostCredentials for NoCredentials {
    fn decode(parts: &DriverParts<'_>) -> std::result::Result<Self, String> {
        if parts.migrator_role.is_some() || parts.applied_by.is_some() {
            return Err(
                "migratorRole and appliedBy are not fields of a status driver: status records \
                 no journal row for a label to name, and takes no narrower identity to \
                 reconcile under"
                    .to_string(),
            );
        }
        Ok(Self)
    }
}

/// A verb whose host driver may narrow its identity but takes no label: `rollback`.
///
/// The label is not missing from the rollback wire; it is not a DRIVER field. Both
/// rollback drivers journal their `rolled_back` events under the request's own
/// `appliedBy`, so putting it here would give the same value two spellings and let
/// one driver read one of them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoleOnly {
    /// The role to `SET ROLE` under while the reverse DDL runs.
    pub migrator_role: Option<String>,
}

impl HostCredentials for RoleOnly {
    fn decode(parts: &DriverParts<'_>) -> std::result::Result<Self, String> {
        if parts.applied_by.is_some() {
            return Err(
                "appliedBy is not a rollback driver field: both drivers journal the rolled_back \
                 events under the request's own appliedBy, so it rides beside the target rather \
                 than on one driver"
                    .to_string(),
            );
        }
        Ok(Self {
            migrator_role: parts.migrator_role.map(str::to_string),
        })
    }
}

/// A verb whose host driver both narrows its identity and supplies the audit label
/// the journal records: `applyIr`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoleAndLabel {
    /// The role to `SET ROLE` under for a least-privilege apply.
    pub migrator_role: Option<String>,
    /// The audit label journalled against every applied step.
    pub applied_by: String,
}

impl HostCredentials for RoleAndLabel {
    fn decode(parts: &DriverParts<'_>) -> std::result::Result<Self, String> {
        let applied_by = parts.applied_by.ok_or_else(|| {
            format!("the {HOST_DRIVER_KIND:?} apply driver requires an appliedBy audit label")
        })?;
        Ok(Self {
            migrator_role: parts.migrator_role.map(str::to_string),
            applied_by: applied_by.to_string(),
        })
    }
}

/// Who owns the database connection a verb runs over, resolved against the dialect
/// it targets and carrying the credentials that verb's host driver takes.
///
/// The driver and the dialect are INDEPENDENT axes. The driver says which side
/// opens the connection; the dialect says which vendor's backend is built over it.
/// Splitting a verb per vendor instead would read as a dialect distinction and be
/// none: SQLite is simply the dialect with no JavaScript driver, which is why it is
/// the dialect the addon opens itself. [`DriverTarget::resolve`] is the single place
/// the pairing is checked - for every verb, not one - so an unserved combination
/// refuses with the pair named instead of being reinterpreted as whichever axis the
/// caller got right.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DriverTarget<C> {
    /// The JS host-driver callback owns the connection; the dialect selects the
    /// `MigrationBackend` built over that seam.
    Host {
        /// The vendor backend the seam is driven as.
        dialect: ApplyDialect,
        /// What this verb's host driver is allowed to narrow to and to label.
        credentials: C,
    },
    /// The addon opens the hardened application connection on its own engine
    /// worker thread. Bundled rusqlite is the only backend it can.
    InProcessSqlite {
        /// The application database file. The journal that describes it lives
        /// inside it, so this is the whole of what the driver opens.
        app_path: String,
    },
}

/// The `driver` fields [`DriverTarget::resolve`] reads, borrowed from the request,
/// plus whether the caller actually passed a host-driver callback.
///
/// The callback is a POSITIONAL argument rather than a request field, so nothing in
/// the request alone can tell a host-driven verb missing its driver from an
/// in-process one carrying a spurious driver. Carrying its presence here is what
/// lets one decoder refuse both.
#[derive(Debug, Clone, Copy)]
pub struct DriverParts<'a> {
    /// `"host"` or `"inProcess"`.
    pub kind: &'a str,
    /// The in-process application database file.
    pub app_path: Option<&'a str>,
    /// The host driver's least-privilege role.
    pub migrator_role: Option<&'a str>,
    /// The host driver's audit label.
    pub applied_by: Option<&'a str>,
    /// Whether a host-driver callback was passed alongside the request.
    pub host_driver_supplied: bool,
}

impl<C: HostCredentials> DriverTarget<C> {
    /// Resolve the requested driver and dialect into the one connection this addon
    /// can actually open for the verb, or say why the pair has none.
    ///
    /// # Errors
    /// Returns the refusal message for an unknown driver kind, a driver whose
    /// callback argument contradicts it, a field belonging to the other driver or to
    /// no driver of this verb, and a driver/dialect pairing the addon does not serve.
    pub fn resolve(dialect: &str, parts: &DriverParts<'_>) -> std::result::Result<Self, String> {
        match parts.kind {
            HOST_DRIVER_KIND => {
                if !parts.host_driver_supplied {
                    return Err(format!(
                        "the {HOST_DRIVER_KIND:?} driver requires a host-driver callback argument"
                    ));
                }
                if parts.app_path.is_some() {
                    return Err(format!(
                        "the {HOST_DRIVER_KIND:?} driver opens no files; appPath belongs to \
                         the {IN_PROCESS_DRIVER_KIND:?} driver"
                    ));
                }
                Ok(Self::Host {
                    dialect: ApplyDialect::parse(dialect)?,
                    credentials: C::decode(parts)?,
                })
            }
            IN_PROCESS_DRIVER_KIND => {
                if parts.host_driver_supplied {
                    return Err(format!(
                        "the {IN_PROCESS_DRIVER_KIND:?} driver opens its own connections and \
                         takes no host-driver callback argument"
                    ));
                }
                // Refused here rather than through `C`, because this half does not
                // vary by verb: no in-process driver of any of them carries either
                // field, so a per-verb answer would be three spellings of one rule.
                if parts.migrator_role.is_some() || parts.applied_by.is_some() {
                    return Err(format!(
                        "the {IN_PROCESS_DRIVER_KIND:?} driver carries neither migratorRole nor \
                         appliedBy: it opens the only connection there is, so there is no second \
                         identity to narrow to, and the label a verb journals under is never one \
                         this driver supplies"
                    ));
                }
                let Some(app_path) = parts.app_path else {
                    return Err(format!(
                        "the {IN_PROCESS_DRIVER_KIND:?} driver requires appPath"
                    ));
                };
                // Compared against the one in-process backend rather than looked up
                // in the dialect table: the served set here has exactly one member,
                // so naming it IS the complete diagnostic. A registered dialect the
                // addon cannot open in-process (postgres) and an unregistered name
                // (duckdb) both learn the same whole answer from this message, which
                // is why they do not need the two-tier refusal
                // [`ApplyDialect::parse`] owes its caller.
                if dialect != SQLITE.as_str() {
                    return Err(format!(
                        "the {IN_PROCESS_DRIVER_KIND:?} driver serves only the {SQLITE} \
                         dialect, not {dialect:?}"
                    ));
                }
                Ok(Self::InProcessSqlite {
                    app_path: app_path.to_string(),
                })
            }
            unknown => Err(format!(
                "unknown driver kind {unknown:?} (expected {HOST_DRIVER_KIND:?} or \
                 {IN_PROCESS_DRIVER_KIND:?})"
            )),
        }
    }
}

/// Split the ordered authored sequence a request carries into the prefix that must
/// already be journalled and the one migration a host-driven apply deploys.
///
/// The two drivers read the same sequence differently, and this is where that is
/// stated. The in-process driver hands the whole sequence to the engine's deploy
/// loop, which applies every envelope the journal does not already carry. The host
/// driver applies ONLY the last, using the prefix to reconstruct declared logical
/// column contracts and refusing unless those plans are proven fully applied. So an
/// empty sequence is a legal no-op deploy for the first and has no migration at all
/// for the second.
///
/// # Errors
/// Returns a refusal when the sequence is empty.
pub fn split_host_envelopes<T>(envelopes: &[T]) -> std::result::Result<(&[T], &T), String> {
    envelopes.split_last().map_or_else(
        || {
            Err(
                "a host-driven apply needs at least one migration envelope; the last entry is \
                 the migration being applied"
                    .to_string(),
            )
        },
        |(current, priors)| Ok((priors, current)),
    )
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
) -> std::result::Result<zeroship_migrate::EffectivePolicy, String> {
    let layers = charter_layer_refs(charter_layers);
    zeroship_migrate::effective_policy_from_charter_layers(&layers)
}

/// Map the wire dialect spelling to the render dialect. Unlike
/// [`ApplyDialect::parse`] this accepts `"sqlite"`: an offline render needs no
/// host driver.
pub fn preview_dialect(s: &str) -> std::result::Result<DialectId, String> {
    shipping_backends()
        .iter()
        .find(|descriptor| descriptor.id.as_str() == s)
        .map(|descriptor| descriptor.id.clone())
        .ok_or_else(|| format!("unknown dialect {s:?} (expected postgres|sqlite|mysql)"))
}

/// Project an [`ApplyOutcome`] and the lock-coherent outstanding rename set into
/// the typed [`ApplyReply`].
pub fn apply_reply(
    outcome: ApplyOutcome,
    pending_contracts: &[zeroship_migrate::PendingContract],
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
        interrupted_unwinds: Vec::new(),
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

fn pending_contract_status_dto(
    contract: &zeroship_migrate::ops::status::PendingContractStatus,
) -> PendingContractStatusDto {
    PendingContractStatusDto {
        table: contract.table.clone(),
        pending_version: contract.pending_version.clone(),
        orphaned: contract.orphaned,
        reason: Some(
            zeroship_migrate::PendingContractRefusal::new(
                contract.table.clone(),
                contract.pending_version.clone(),
            )
            .to_string(),
        ),
    }
}

fn blocked_plan_dto(blocked: &zeroship_migrate::ops::status::BlockedPlan) -> BlockedPlanDto {
    BlockedPlanDto {
        blocked: blocked.blocked.as_str().to_string(),
        dependency: blocked.dependency.as_str().to_string(),
        pending_version: blocked.pending_version.clone(),
        reason: Some(
            zeroship_migrate::DependencyPendingContract::new(
                blocked.blocked.as_str(),
                blocked.dependency.as_str(),
                blocked.pending_version.clone(),
            )
            .to_string(),
        ),
    }
}

/// Project a [`MigrationStatus`] into the typed [`StatusReply`] (the load-bearing
/// fields: current version + applied/pending/rolled-back version ids).
pub fn status_reply(s: &MigrationStatus) -> StatusReply {
    StatusReply {
        // Filled by the caller that holds the backend; the projection itself
        // has no connection to read the marker table with.
        interrupted_unwinds: Vec::new(),
        current_version: s.current_version.as_ref().map(|v| v.as_str().to_string()),
        applied: s.applied.iter().map(|e| e.version.clone()).collect(),
        pending: s.pending.iter().map(|v| v.as_str().to_string()).collect(),
        aborted: Vec::new(),
        rolled_back: s.rolled_back.iter().map(|e| e.version.clone()).collect(),
        pending_contracts: s
            .pending_contracts
            .iter()
            .map(pending_contract_status_dto)
            .collect(),
        blocked: s.blocked.iter().map(blocked_plan_dto).collect(),
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
            touched_tables: None,
        })
        .collect();
    StatusReply {
        interrupted_unwinds: Vec::new(),
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
            .map(pending_contract_status_dto)
            .collect(),
        blocked: status.blocked.iter().map(blocked_plan_dto).collect(),
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
#[allow(clippy::too_many_arguments)]
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
    // The project lock comes FIRST, so it serializes the journal bootstrap too.
    //
    // The bootstrap needs that serialization: on a first deploy the journal
    // namespace, its types, its table and its triggers do not exist yet, and
    // PostgreSQL's `CREATE ... IF NOT EXISTS` is racy exactly there. Without
    // the lock two processes both find them absent and both try to create
    // them, and the loser surfaces a raw catalog error -- `duplicate key value
    // violates unique constraint "pg_type_typname_nsp_index"`, `tuple
    // concurrently updated`, or a trigger that "already exists" -- which reads
    // like corruption but is only contention.
    //
    // Nothing in the acquisition needs the journal: it is an advisory lock
    // keyed on the project id. Taking it first also means a failed acquisition
    // leaves no journal objects behind for a deploy that never ran.
    backend
        .acquire_project_lock(cfg)
        .await
        .map_err(|error| format!("failed to acquire project lock: {error}"))?;

    let result = async {
        // Inside the bracket on purpose: the lock is released after this block, so
        // bootstrapping outside it would leak the lock whenever the bootstrap failed.
        backend
            .ensure_journal(cfg)
            .await
            .map_err(|error| error.to_string())?;
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
            let status = zeroship_migrate::ops::status::status_plans_via_backend_locked(
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
        let outcome = MigrationEngine::new(zeroship_migrate::shipping_vendors())
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
            let id = zeroship_migrate::model::migration::MigrationId::parse(version)
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
    /// One rollback-selection identity per artifact that lowered to exactly ONE
    /// journaled step. Rich data steps use a metadata-only `Migration` here; their
    /// executable reverse remains structured in `inverse_plans`.
    migrations: Vec<Migration>,
    /// FORWARD journal version to the author's lowered inverse plan.
    inverse_plans: std::collections::BTreeMap<String, zeroship_migrate::AppliedPlan>,
    /// Journaled step identity to the authored migration that owns it, for every
    /// step of a plan that lowered to more than one. These are the identities the
    /// engine refuses as `MissingFromSet`, and the map is what turns that refusal
    /// from an opaque derived version into a name the operator authored.
    unrepresentable: std::collections::BTreeMap<String, RollbackRefusal>,
    /// Logical PLAN version to the journaled STEP version it lowered to, for the
    /// single-step plans above.
    ///
    /// The two are different `mig_...` values for the same migration, and `status`
    /// reports the plan one while the journal, `apply` and `rollback` speak the
    /// step one. Without this map every version `status` listed as applied was
    /// rejected by `--to` as "not currently applied", which made the obvious
    /// workflow -- read a version from `status`, roll back to it -- impossible.
    ///
    /// Only single-step plans are recorded, which is what makes the mapping
    /// unambiguous: a multi-step plan is refused as `unrepresentable` before
    /// rollback can act on it, so for everything reachable here plan and step
    /// stand 1:1.
    plan_to_step: std::collections::BTreeMap<String, String>,
    /// Applied pre-upgrade rows whose NULL stored reverse made this request use
    /// the checksummed source-derived compatibility path.
    reconstructed_reverses: std::collections::BTreeSet<String>,
}

enum RollbackRefusal {
    MultiStep { name: String },
    DeclaredIrreversible { name: String, reason: String },
    MissingInverse { name: String, kind: String },
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
    artifacts: &[zeroship_migrate::LoweredArtifact],
    journal_entries: &[zeroship_migrate::apply::journal::AppliedEntry],
) -> std::result::Result<RollbackSet, String> {
    let mut set = RollbackSet {
        migrations: Vec::with_capacity(artifacts.len()),
        inverse_plans: std::collections::BTreeMap::new(),
        unrepresentable: std::collections::BTreeMap::new(),
        plan_to_step: std::collections::BTreeMap::new(),
        reconstructed_reverses: std::collections::BTreeSet::new(),
    };
    for artifact in artifacts {
        // The manifest is the one walker that already enumerates every step
        // variant's journal identity, so the refusal can name the step kind
        // without teaching this module the shape of each variant.
        //
        // BEFORE MAKING MULTI-STEP PLANS REVERSIBLE, READ THIS. A second,
        // unrelated safety property currently rests on this refusal, and removing
        // it here would break that one silently.
        //
        // On MySQL an interrupted unwind leaves a marker in
        // `schema_migrations_rollback_inflight`, and `apply` deliberately does NOT
        // consult that marker - it sees the migration still journaled `applied`
        // and skips it. That is correct ONLY because everything reachable here
        // lowers to exactly one journaled step, and a single DDL statement either
        // ran or did not: there is no half-reverted shape for `apply` to mistake
        // for an intact one.
        //
        // Make several steps reversible and that stops holding. An unwind can then
        // fail after step 1 committed, leaving a half-reverted schema under an
        // `applied` row, which `apply` will skip. Whoever does that work has to
        // decide what `apply` does when a rollback marker is present.
        //
        // No existing test covers the combination: the interrupted-unwind path is
        // pinned for the single-step case
        // (`mysql-interrupted-rollback-recovery.test.ts`) and this refusal is
        // pinned on its own (`rollback-data-semantics.test.ts`), so both suites stay
        // green while the property between them disappears.
        let manifest = PlanStatusManifest::from_applied_plan(&artifact.plan, &artifact.depends_on)
            .map_err(|error| error.to_string())?;
        let [forward] = manifest.steps.as_slice() else {
            for step in &manifest.steps {
                set.unrepresentable.insert(
                    step.version.as_str().to_string(),
                    RollbackRefusal::MultiStep {
                        name: manifest.name.clone(),
                    },
                );
            }
            continue;
        };

        // Recorded only for the exactly-one-journal-identity case, on purpose:
        // resolving a logical plan id to its step id cannot be ambiguous here.
        set.plan_to_step.insert(
            artifact.plan.version.as_str().to_string(),
            forward.version.as_str().to_string(),
        );

        if let Some(inverse_plan) = &artifact.inverse_plan {
            // Selection/checksum/dependency gates still operate on a Migration
            // identity. No textual reverse is fabricated: execution looks this
            // forward id up in `inverse_plans` and keeps template + binds native.
            set.migrations.push(Migration {
                version: forward.version.clone(),
                name: forward.name.clone(),
                up: String::new(),
                down: None,
                checksum: forward.checksum.clone(),
                flags: artifact.plan.flags,
                owner_app: artifact.plan.owner_app.clone(),
                depends_on: artifact.plan.depends_on.clone(),
                supersedes: artifact.plan.supersedes.clone(),
                preconditions: artifact.plan.preconditions.clone(),
                existence_guard: None,
                effect: None,
            });
            set.inverse_plans
                .insert(forward.version.as_str().to_string(), inverse_plan.clone());
            continue;
        }

        if let Ok(migration) = artifact.plan.single_step_migration() {
            let mut migration = migration.clone();
            if let Some(entry) = journal_entries
                .iter()
                .find(|entry| entry.version == forward.version.as_str())
            {
                if let Some(stored_down) = &entry.down {
                    // Replace only the executable reverse. The migration's
                    // freshly-lowered checksum stays intact so the existing
                    // source-vs-journal drift refusal remains authoritative.
                    migration.down = Some(stored_down.clone());
                } else if migration.down.is_some() {
                    set.reconstructed_reverses
                        .insert(forward.version.as_str().to_string());
                }
            }
            set.migrations.push(migration);
            continue;
        }

        let refusal = match &artifact.irreversible {
            Some(reason) => RollbackRefusal::DeclaredIrreversible {
                name: manifest.name.clone(),
                reason: reason.clone(),
            },
            None => RollbackRefusal::MissingInverse {
                name: manifest.name.clone(),
                kind: forward.kind.as_str().to_string(),
            },
        };
        set.unrepresentable
            .insert(forward.version.as_str().to_string(), refusal);
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
#[allow(clippy::too_many_arguments)]
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
    // The project lock comes FIRST, so it serializes the journal bootstrap too.
    //
    // The bootstrap needs that serialization: on a first deploy the journal
    // namespace, its types, its table and its triggers do not exist yet, and
    // PostgreSQL's `CREATE ... IF NOT EXISTS` is racy exactly there. Without
    // the lock two processes both find them absent and both try to create
    // them, and the loser surfaces a raw catalog error -- `duplicate key value
    // violates unique constraint "pg_type_typname_nsp_index"`, `tuple
    // concurrently updated`, or a trigger that "already exists" -- which reads
    // like corruption but is only contention.
    //
    // Nothing in the acquisition needs the journal: it is an advisory lock
    // keyed on the project id. Taking it first also means a failed acquisition
    // leaves no journal objects behind for a deploy that never ran.
    backend
        .acquire_project_lock(cfg)
        .await
        .map_err(|error| format!("failed to acquire project lock: {error}"))?;

    let result = async {
        // Inside the bracket on purpose: the lock is released after this block, so
        // bootstrapping outside it would leak the lock whenever the bootstrap failed.
        backend
            .ensure_journal(cfg)
            .await
            .map_err(|error| error.to_string())?;
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
            &resolved_contracts,
        )?;
        let set = rollback_migration_set(&artifacts, &journal_entries)?;
        // `--to` accepts EITHER spelling of a migration's version. The engine
        // anchors on the journaled step identity, so a logical plan version is
        // rewritten to the step it lowered to; anything already a step version, or
        // belonging to neither space, passes through untouched and the engine's own
        // `UnknownTarget` refusal still reports it verbatim.
        //
        // Widening the accepted spellings must not widen what is ACCEPTED overall:
        // an unrecognised version is still refused, and still unwinds nothing.
        let target = match target {
            RollbackTarget::ToVersion(version) => {
                let resolved = set
                    .plan_to_step
                    .get(version.as_str())
                    .cloned()
                    .unwrap_or_else(|| version.as_str().to_string());
                RollbackTarget::ToVersion(
                    zeroship_migrate::MigrationId::parse(&resolved).unwrap_or(version),
                )
            }
            other => other,
        };
        let request = zeroship_migrate::RollbackRequest::new(target).with_options(options);
        // The guard the engine's own apply sites use. Composing one from the same
        // charter here would drop the config's host-selected mode.
        let guard = zeroship_migrate::guard_for(zeroship_migrate::shipping_vendors(), &cfg.guard_config_for(&backend.dialect()));
        let outcome = zeroship_migrate::rollback_with_lock_and_inverse_plans(
            backend,
            cfg,
            &request,
            &set.migrations,
            &set.inverse_plans,
            approval,
            applied_by,
            guard.as_ref(),
            LockMode::AlreadyHeld,
        )
        .await
        .map_err(|error| describe_rollback_error(&error, &set))?;
        let advisories = outcome
            .rolled_back
            .iter()
            .filter(|version| set.reconstructed_reverses.contains(*version))
            .map(|version| {
                format!(
                    "reconstructed legacy reverse for {version} from the checksummed migration source because its applied journal row has NULL down"
                )
            })
            .collect();
        Ok::<RollbackReply, String>(RollbackReply {
            rolled_back: outcome.rolled_back,
            skipped_irreversible: outcome.skipped_irreversible,
            advisories,
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
///
/// The engine raises `MissingFromSet` for two situations it cannot tell apart, and
/// only one of them is the situation that variant is named for. A version really
/// absent from the supplied set keeps the engine's wording. A version the caller DID
/// supply, whose plan lowered to more than one journaled step, gets its own message:
/// it is in `unrepresentable`, which exists precisely because the engine reduced it
/// to a per-step identity it can no longer match.
///
/// The second message REPLACES the engine's rather than appending to it: an
/// appended name would read "migration mig_... is applied but absent from the
/// supplied set ... That version is `<name>`" - a sentence that denies the
/// migration was supplied and then names it from the supplied set two clauses
/// later. An operator reading the first half goes looking for a migration file
/// that is not missing.
fn describe_rollback_error(error: &zeroship_migrate::RollbackError, set: &RollbackSet) -> String {
    let zeroship_migrate::RollbackError::MissingFromSet { version } = error else {
        return error.to_string();
    };
    set.unrepresentable.get(version.as_str()).map_or_else(
        || error.to_string(),
        |refusal| match refusal {
            RollbackRefusal::MultiStep { name } => format!(
                "migration {name} cannot be rolled back: it lowers to more than one \
                 journaled step. Only a migration that lowers to exactly ONE journaled \
                 step is reversible; otherwise an interrupted MySQL unwind could leave \
                 a half-reverted shape still journaled as applied. Roll forward with a \
                 compensating migration instead"
            ),
            RollbackRefusal::DeclaredIrreversible { name, reason } => format!(
                "migration {name} cannot be rolled back: the author declared it \
                 irreversible: {reason}"
            ),
            RollbackRefusal::MissingInverse { name, kind } => format!(
                "migration {name} cannot be rolled back: its single {kind} step carries \
                 no recorded inverse"
            ),
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
#[allow(clippy::too_many_arguments)]
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
    // The lock comes first here too. A non-read-only status BOOTSTRAPS the
    // journal, so it must hold the lock before bootstrapping: without it the
    // bootstrap races a concurrent deploy on a fresh project, and the raw
    // catalog error lands on the DEPLOY as readily as on the status.
    //
    // Still non-blocking: a contended acquisition returns the busy reply having
    // bootstrapped nothing, which is the honest answer for a reader that arrived
    // mid-deploy.
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
        // Inside the bracket: the release runs after this block, so bootstrapping
        // outside it would leak the lock whenever the bootstrap failed.
        if !read_only {
            backend
                .ensure_journal(cfg)
                .await
                .map_err(|error| error.to_string())?;
        }
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
        let touched_by_plan: std::collections::HashMap<String, Vec<String>> = artifacts
            .iter()
            .map(|artifact| {
                (
                    artifact.plan.version.as_str().to_string(),
                    artifact.touched_tables.clone(),
                )
            })
            .collect();
        let manifests = artifacts
            .iter()
            .map(|artifact| {
                PlanStatusManifest::from_applied_plan(&artifact.plan, &artifact.depends_on)
                    .map_err(|error| error.to_string())
            })
            .collect::<std::result::Result<Vec<_>, _>>()?;
        let status = if read_only {
            zeroship_migrate::ops::status::status_plans_via_backend_read_only_locked(
                backend, cfg, &manifests,
            )
            .await
        } else {
            zeroship_migrate::ops::status::status_plans_via_backend_locked(backend, cfg, &manifests)
                .await
        }
        .map_err(|error| error.to_string())?;
        // The state apply and rollback both refuse over. Status reporting a clean
        // project while they refuse leaves the operator with a contradiction and
        // nothing to act on, so it is read here and carried in the reply.
        // The hook returns nothing on the dialects that cannot leave the marker.
        // The seam hands back each marker WITH the owning backend's instruction for
        // clearing it. This reply carries versions only, so the instruction is dropped
        // here rather than in the contract: an operator who needs it gets it from the
        // `apply` refusal, which is where it is actionable.
        let interrupted_unwinds: Vec<String> = backend
            .unresolved_rollback_markers(cfg)
            .await
            .map_err(|error| error.to_string())?
            .into_iter()
            .map(|marker| marker.version)
            .collect();
        let mut reply = plan_status_reply(&status);
        for plan in reply.plans.iter_mut().flatten() {
            plan.touched_tables =
                Some(touched_by_plan.get(&plan.version).cloned().ok_or_else(|| {
                    format!(
                        "status returned plan {} without its lowered touched-table set",
                        plan.version
                    )
                })?);
        }
        reply.interrupted_unwinds = interrupted_unwinds;
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
    // Lock before bootstrap, for the reason the plan-aware verb above documents:
    // bootstrapping first races a concurrent deploy on a fresh project.
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
        // Inside the bracket, so a bootstrap failure still releases the lock.
        backend
            .ensure_journal(cfg)
            .await
            .map_err(|error| error.to_string())?;
        let status = zeroship_migrate::ops::status::status_via_backend_locked(backend, cfg, migrations)
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
    resolution: zeroship_migrate::Resolution,
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
        let outcome = MigrationEngine::new(zeroship_migrate::shipping_vendors())
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


/// One records-not-run journal event an adoption would write, owning its own
/// strings so the borrowed [`journal::BaselineRecord`] set can be built from it.
struct AdoptionRow {
    version: String,
    name: String,
    checksum: String,
    /// `'squash'` for the one event that carries the supersession edges,
    /// `'baseline'` for every other. The distinction is load-bearing:
    /// `superseded_versions` honours edges only from a NET-APPLIED event whose
    /// recorded `kind` is `'squash'`, so stamping every row `'baseline'` would
    /// write edges nothing reads.
    kind: &'static str,
    supersedes: Vec<String>,
}

/// Adopt an existing database: record what applying the authored set from nothing
/// would have journaled, WITHOUT running any of it, inside one project-lock bracket.
///
/// # Why the plans are lowered against an EMPTY schema
///
/// Every other verb lowers against the live catalog. This one cannot, and the
/// reason is the situation it exists for. Adoption is invoked on a database whose
/// schema is ALREADY the corpus's output and whose journal does not say so, and
/// lowering an unjournaled `createTable` against a catalog that already holds the
/// table fails the pending-schema projection outright ("failed to project pending
/// schema"). The identities this verb must journal would be unreachable behind that
/// refusal - the lowering needed to compute them is the lowering the adopted state
/// breaks.
///
/// So the basis is `SchemaSnapshot::default()` with an empty journal: exactly what
/// a FRESH apply of the same ordered envelopes sees, which is exactly the history
/// the operator is asserting already ran. The identities that basis produces are
/// the ones a later `status` reconciles against, because once these events exist
/// every step has completed journal evidence and status no longer projects anything.
///
/// # What is verified, and what is asserted
///
/// VERIFIED, under the lock: the corpus's projected final schema is folded from the
/// same ops (`fold_ops_onto` over an empty base) and STRUCTURALLY COMPARED against
/// the live catalog by `zeroship_migrate::diff_snapshots`, which reaches columns,
/// indexes, constraints, sequences, views, roles and extensions - see
/// [`assert_corpus_output_is_live`] for which classes of difference refuse and why
/// one of them is tolerated; no step may already be journaled under a DIFFERENT
/// checksum; no step may be mid-flight; and journal rows the corpus does not account
/// for are reported by name.
///
/// ASSERTED BY THE OPERATOR, and NOT checked: the DATA. A schema that matches
/// column for column can still hold rows a migration's backfill never wrote, and
/// nothing here looks at a row. The structural check also tolerates objects the live
/// catalog has and the corpus does not, which is what lets a trailing migration that
/// only DROPS slip through. That residual is why the caller still gates this on an
/// explicit approval and why the reply enumerates every event before it is written.
#[allow(clippy::too_many_arguments)]
pub async fn baseline_ir_with_locked_backend<B: MigrationBackend>(
    backend: &B,
    cfg: &ExecutorConfig,
    envelope_json: &[String],
    owner_app: &str,
    project_schema: &str,
    dialect: &str,
    registry_json: &str,
    charter_layers: &[String],
    supersede_unmatched: bool,
    dry_run: bool,
    applied_by: &str,
) -> std::result::Result<BaselineReply, String> {
    let charter_refs = charter_layer_refs(charter_layers);
    // Waiting, not try-acquire: adoption WRITES, so it is a peer of the deploy it
    // would race rather than a reader that can decline and report busy.
    backend
        .acquire_project_lock(cfg)
        .await
        .map_err(|error| format!("failed to acquire project lock: {error}"))?;

    let result = async {
        // Capability probe, FIRST and DB-free in effect: an empty adoption writes
        // nothing on a backend that implements the records-not-run write and is
        // refused outright by one that does not. Asked here rather than after the
        // work so a backend that cannot adopt never shows an operator a preview it
        // could never apply -- a dry run is otherwise indistinguishable from a
        // supported one right up to the write.
        backend.record_adoption(cfg, &[]).await.map_err(|error| {
            format!("this backend cannot adopt an existing project: {error}")
        })?;
        backend
            .ensure_journal(cfg)
            .await
            .map_err(|error| error.to_string())?;

        // The identities a FRESH apply of this corpus would journal. See the doc
        // above for why the basis is empty rather than live.
        let artifacts = crate::lower::lower_ordered_envelopes_to_plans(
            envelope_json,
            owner_app,
            project_schema,
            dialect,
            registry_json,
            &charter_refs,
            zeroship_migrate::model::snapshot::SchemaSnapshot::default(),
            &[],
            &[],
        )?;
        let manifests = artifacts
            .iter()
            .map(|artifact| {
                PlanStatusManifest::from_applied_plan(&artifact.plan, &artifact.depends_on)
                    .map_err(|error| error.to_string())
            })
            .collect::<std::result::Result<Vec<_>, _>>()?;

        assert_corpus_output_is_live(backend, cfg, envelope_json, dialect, project_schema, charter_layers)
            .await?;

        // The SAME reconciliation `status` reports, against the SAME journal, inside
        // the SAME lock. The unmatched set below is therefore `status`'s own
        // `unexpectedJournal` rather than a second query that could disagree with the
        // verdict the operator read before running this.
        let status = zeroship_migrate::ops::status::status_plans_via_backend_locked(
            backend, cfg, &manifests,
        )
        .await
        .map_err(|error| error.to_string())?;

        let mut state_by_step: std::collections::HashMap<&str, zeroship_migrate::ops::status::PlanStatusStepState> =
            std::collections::HashMap::new();
        for plan in &status.plans {
            for step in &plan.steps {
                state_by_step.insert(step.version.as_str(), step.state);
            }
        }

        let mut rows: Vec<AdoptionRow> = Vec::new();
        let mut already_recorded: Vec<String> = Vec::new();
        for manifest in &manifests {
            for step in &manifest.steps {
                let version = step.version.as_str();
                let state = state_by_step.get(version).copied().ok_or_else(|| {
                    format!("status omitted step {version} of plan {}", manifest.name)
                })?;
                match state {
                    zeroship_migrate::ops::status::PlanStatusStepState::Applied => {
                        already_recorded.push(version.to_string());
                    }
                    zeroship_migrate::ops::status::PlanStatusStepState::Pending => {
                        rows.push(AdoptionRow {
                            version: version.to_string(),
                            name: step.name.clone(),
                            checksum: step.checksum.as_str().to_string(),
                            kind: "baseline",
                            supersedes: Vec::new(),
                        });
                    }
                    // Everything else is a fact about this database that adoption
                    // must not paper over. Drift means the journal already holds this
                    // identity under different bytes; inflight means an apply of it
                    // was interrupted; aborted means an online rename was explicitly
                    // unwound. Recording "applied" over any of them would destroy the
                    // evidence, and the journal is append-only, so there is no undo.
                    other => {
                        return Err(format!(
                            "cannot adopt {}: its step {} ({}) is already journaled as {}. \
                             Adoption records history that is absent from the journal; it never \
                             overwrites history that is present. Resolve that step first",
                            manifest.name,
                            step.name,
                            version,
                            other.as_str()
                        ))
                    }
                }
            }
        }

        let mut unmatched: Vec<String> = Vec::new();
        for entry in &status.unexpected_journal {
            if entry.state != zeroship_migrate::ops::status::PlanStatusStepState::Applied {
                return Err(format!(
                    "cannot adopt this project: journal entry {} is {}, not a settled applied \
                     event. An interrupted apply is a repair, not something an adoption may \
                     supersede",
                    entry.version,
                    entry.state.as_str()
                ));
            }
            unmatched.push(entry.version.clone());
        }

        // The edges ride the FIRST event this adoption writes, not the last. They
        // count only while their carrier is net-applied (`superseded_versions`
        // restricts to a net-applied `kind='squash'`), and the oldest event in a
        // project's recorded history is the one a later rollback is least likely to
        // reach - so the pre-adoption journal stays explained for as long as the
        // adoption itself stands.
        let superseded: Vec<String> = if supersede_unmatched { unmatched.clone() } else { Vec::new() };
        if !superseded.is_empty() {
            let Some(first) = rows.first_mut() else {
                return Err(format!(
                    "every step of this migration set is already journaled, so there is no new \
                     event to carry the supersession of {} unmatched journal row(s). The journal \
                     is append-only: edges can only be attached to an event being written",
                    unmatched.len()
                ));
            };
            first.kind = "squash";
            first.supersedes = superseded.clone();
        }

        let wrote = !dry_run && !rows.is_empty() && (unmatched.is_empty() || supersede_unmatched);
        if wrote {
            let edge_refs: Vec<Vec<&str>> = rows
                .iter()
                .map(|row| row.supersedes.iter().map(String::as_str).collect())
                .collect();
            let records: Vec<zeroship_migrate::apply::journal::BaselineRecord<'_>> = rows
                .iter()
                .zip(&edge_refs)
                .map(|(row, edges)| zeroship_migrate::apply::journal::BaselineRecord {
                    version: &row.version,
                    name: &row.name,
                    checksum: &row.checksum,
                    applied_by,
                    kind: row.kind,
                    supersedes: edges.as_slice(),
                })
                .collect();
            backend
                .record_adoption(cfg, &records)
                .await
                .map_err(|error| error.to_string())?;
        }

        Ok::<BaselineReply, String>(BaselineReply {
            recorded: rows
                .iter()
                .map(|row| BaselineStepDto {
                    version: row.version.clone(),
                    name: row.name.clone(),
                    kind: row.kind.to_string(),
                })
                .collect(),
            already_recorded,
            unmatched,
            // The edges this adoption WOULD write, not the ones it did. `recorded`
            // is already reported that way (`wire.rs`), and this field was the one
            // exception: emptied on a dry run, it left a preview announcing a
            // `kind: "squash"` event - whose entire meaning is its edges - beside an
            // empty edge list, so the single most irreversible part of the operation
            // was the one part the preview omitted. `wrote` below is what separates
            // a preview from a write, and it is now the ONLY field that does.
            superseded,
            wrote,
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

/// Refuse an adoption whose corpus does not describe the database it is pointed at.
///
/// The worst realistic misuse of a records-not-run verb is pointing it at the WRONG
/// database - an empty one, or a peer environment - which journals a whole history
/// as applied over a schema that was never built. Nothing later repairs that: every
/// migration reads as done, `apply` runs none of them, and the journal is
/// append-only.
///
/// The corpus's projected final schema is folded from the same ops through the
/// engine's own `fold_ops_onto` over an empty base, so a table a later migration
/// DROPS or RENAMES is correctly absent from the expectation rather than demanded.
/// A fold that cannot replay is a refusal, not a skip: an unverifiable adoption is
/// exactly the one not to wave through.
///
/// The comparison is [`zeroship_migrate::diff_snapshots`] - the same structural
/// differ `status`'s drift surface runs - so it reaches columns, indexes,
/// constraints, sequences, views, roles and extensions rather than table names. A
/// name-only comparison would catch the wrong database and NOT the subtly wrong
/// one: a peer environment whose trailing migrations only ALTER has every table
/// name present, so adoption would journal those ALTERs as applied and the columns
/// the app needs never arrive.
///
/// # Which classes of difference are fatal
///
/// **`missing` and `altered` refuse.** Both mean the live schema is not what this
/// corpus produces, and adoption's whole effect is to guarantee nothing will ever
/// fix that. `diff_snapshots` is the differ the `fold_live` suites
/// (`crates/zeroship-migrate/tests/fold_live/`) already require to report
/// `is_clean()` for a corpus `PostgreSQL` really applied, so a clean verdict here is
/// the same verdict a real apply earns.
///
/// **`unexpected` is TOLERATED**, and the reason is that adoption must be possible.
/// A live database legitimately carries objects this corpus never authored - an
/// out-of-band table, a column a DBA added, an index another tool built - and
/// refusing those refuses every real adoption, which only teaches an operator to
/// look for a bypass.
///
/// WHAT THAT GIVES UP, and it is not nothing: a trailing migration that only DROPS
/// lands in `unexpected` rather than `missing`. A peer environment one migration
/// behind a `dropColumn` shows the surviving column as unexpected, adoption records
/// the drop as applied, and the column stays forever - the same harm as the
/// never-added column above, arriving through the tolerated bucket. Separating "an
/// object the corpus dropped" from "an object the corpus never knew" needs the set
/// of names the ops TOUCHED, which a folded final snapshot does not carry; closing
/// it means threading that set out of the fold. Until then this refusal is
/// one-sided by construction, and saying so is the point.
///
/// Note that roles, schemas and extensions are only ever reported as `missing` by
/// `diff_snapshots` (it pushes no `unexpected` for them), so tolerating `unexpected`
/// costs nothing on those classes - a live catalog's extra roles and extensions were
/// never going to be reported in the first place.
async fn assert_corpus_output_is_live<B: MigrationBackend>(
    backend: &B,
    cfg: &ExecutorConfig,
    envelope_json: &[String],
    dialect: &str,
    project_schema: &str,
    charter_layers: &[String],
) -> std::result::Result<(), String> {
    let dialect_id = preview_dialect(dialect)?;
    let effective = effective_policy_from_wire_layers(charter_layers)?;
    let mut ops = Vec::new();
    for envelope in envelope_json {
        let ir: zeroship_migrate::model::ir::MigrationIr = serde_json::from_str(envelope)
            .map_err(|error| format!("envelope is not a MigrationIr document: {error}"))?;
        ops.extend(ir.ops);
    }
    let projected = zeroship_migrate::fold_ops_onto(
        zeroship_migrate::shipping_vendors(),
        &zeroship_migrate::model::snapshot::SchemaSnapshot::default(),
        &ops,
        &dialect_id,
        project_schema,
        &effective,
    )
    .map_err(|error| {
        format!(
            "cannot verify this adoption: the migration set does not replay from an empty \
             schema, so the shape it claims to have produced is unknown ({error})"
        )
    })?;
    let live = backend
        .snapshot_schema(cfg)
        .await
        .map_err(|error| format!("live schema introspection failed: {error}"))?;
    let drift =
        zeroship_migrate::diff_snapshots(zeroship_migrate::shipping_vendors(), &projected, &live);
    if let Some(refusal) = adoption_drift_refusal(project_schema, &drift) {
        return Err(refusal);
    }
    Ok(())
}

/// How many differences of ONE class the refusal spells out before it stops naming
/// and starts counting.
///
/// The two databases this refusal separates are at opposite ends of the range. A
/// stale peer environment differs by a handful of columns and is named in full,
/// which is the case the naming exists for. The wrong database entirely differs by
/// everything, and several hundred lines of names tell an operator less than twenty
/// plus a total does. The total is always printed, so the cap hides the identity of
/// some differences but never their existence or their number.
const ADOPTION_DRIFT_SAMPLE: usize = 20;

/// One `class: name` block of the refusal, capped at [`ADOPTION_DRIFT_SAMPLE`].
fn drift_refusal_lines(class: &str, items: &[String], into: &mut String) {
    use std::fmt::Write as _;
    for item in items.iter().take(ADOPTION_DRIFT_SAMPLE) {
        let _ = write!(into, "\n  {class}: {item}");
    }
    if let Some(rest) = items
        .len()
        .checked_sub(ADOPTION_DRIFT_SAMPLE)
        .filter(|n| *n > 0)
    {
        let _ = write!(into, "\n  ... and {rest} more {class}");
    }
}

/// The refusal a structural comparison earns, or `None` when this database is the
/// one the corpus describes.
///
/// Pure, so the classification decision - fail on `missing` and `altered`, tolerate
/// `unexpected` - is stated once and tested without a database. See
/// [`assert_corpus_output_is_live`] for why each class falls where it does.
///
/// The differences are NAMED. An operator running this holds two similar databases
/// and needs to learn which one they are pointed at; "the schema does not match"
/// tells them only that they were right to be unsure.
fn adoption_drift_refusal(
    project_schema: &str,
    drift: &zeroship_migrate::StructuralDrift,
) -> Option<String> {
    let missing = &drift.missing_objects;
    let altered: Vec<String> = drift
        .altered_objects
        .iter()
        .map(|object| {
            format!(
                "{} {}: {} is `{}` here, `{}` in the database",
                object.table, object.object, object.field, object.expected, object.actual
            )
        })
        .collect();
    if missing.is_empty() && altered.is_empty() {
        return None;
    }
    let total = missing.len() + altered.len();
    let mut message = format!(
        "refusing to adopt {project_schema}: this database is not what the migration set \
         produces ({total} difference(s))"
    );
    drift_refusal_lines("missing", missing, &mut message);
    drift_refusal_lines("altered", &altered, &mut message);
    message.push_str(
        "\nAdoption records migrations as applied WITHOUT running them, so nothing would ever \
         apply the differences above: `apply` would report nothing pending, `status --strict` \
         would report clean, and the journal is append-only so there is no undo. This is what \
         a database a few migrations behind looks like - a stale DATABASE_URL, or a peer \
         environment. Apply the set instead, or point this at the database that already has it",
    );
    Some(message)
}

#[cfg(test)]
mod adoption_drift_tests {
    use super::{adoption_drift_refusal, ADOPTION_DRIFT_SAMPLE};
    use zeroship_migrate::{AlteredObject, StructuralDrift};

    fn altered(
        table: &str,
        object: &str,
        field: &str,
        expected: &str,
        actual: &str,
    ) -> AlteredObject {
        AlteredObject {
            table: table.to_string(),
            object: object.to_string(),
            field: field.to_string(),
            expected: expected.to_string(),
            actual: actual.to_string(),
        }
    }

    #[test]
    fn a_matching_database_earns_no_refusal() {
        assert!(adoption_drift_refusal("app", &StructuralDrift::default()).is_none());
    }

    /// THE PEER ENVIRONMENT. Every table name matches and one column is behind, so
    /// the presence-only predicate this replaced saw nothing at all.
    #[test]
    fn a_missing_column_refuses_and_names_itself() {
        let drift = StructuralDrift {
            missing_objects: vec!["users.mfa_secret".to_string()],
            ..StructuralDrift::default()
        };
        let refusal = adoption_drift_refusal("app", &drift).expect("a missing column refuses");
        assert!(refusal.contains("users.mfa_secret"), "{refusal}");
        assert!(refusal.contains("1 difference(s)"), "{refusal}");
    }

    #[test]
    fn an_altered_column_refuses_and_names_the_field_and_both_values() {
        let drift = StructuralDrift {
            altered_objects: vec![altered(
                "users",
                "column id",
                "data_type",
                "integer",
                "bigint",
            )],
            ..StructuralDrift::default()
        };
        let refusal = adoption_drift_refusal("app", &drift).expect("an altered column refuses");
        assert!(refusal.contains("users column id"), "{refusal}");
        assert!(refusal.contains("data_type"), "{refusal}");
        assert!(
            refusal.contains("integer") && refusal.contains("bigint"),
            "{refusal}"
        );
    }

    /// The tolerated class, stated as a test rather than as a comment: a live
    /// catalog carrying objects this corpus never authored is the NORMAL shape of a
    /// database worth adopting, and refusing it refuses every real adoption.
    #[test]
    fn unexpected_objects_alone_do_not_refuse() {
        let drift = StructuralDrift {
            unexpected_objects: vec![
                "legacy_audit".to_string(),
                "users.dba_hotfix".to_string(),
                "sequence some_other_tool_seq".to_string(),
            ],
            ..StructuralDrift::default()
        };
        assert!(adoption_drift_refusal("app", &drift).is_none());
    }

    /// The cap names some differences and counts all of them. A refusal that
    /// silently truncated would understate how wrong the database is, which is the
    /// one thing an operator holding two similar databases must not be misled about.
    #[test]
    fn the_sample_cap_still_reports_the_total() {
        let missing: Vec<String> = (0..ADOPTION_DRIFT_SAMPLE + 5)
            .map(|n| format!("users.c{n}"))
            .collect();
        let drift = StructuralDrift {
            missing_objects: missing,
            ..StructuralDrift::default()
        };
        let refusal = adoption_drift_refusal("app", &drift).expect("missing columns refuse");
        assert!(
            refusal.contains(&format!("{} difference(s)", ADOPTION_DRIFT_SAMPLE + 5)),
            "{refusal}"
        );
        assert!(refusal.contains("and 5 more missing"), "{refusal}");
        assert!(refusal.contains("users.c0"), "{refusal}");
        assert!(
            !refusal.contains(&format!("users.c{}", ADOPTION_DRIFT_SAMPLE + 4)),
            "{refusal}"
        );
    }
}
#[cfg(test)]
mod status_projection_tests {
    use super::*;
    use zeroship_migrate::apply::journal::JournaledKind;
    use zeroship_migrate::model::migration::MigrationId;
    use zeroship_migrate::ops::status::{BlockedPlan, PendingContractStatus, UnexpectedJournalEntry};

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
                state: zeroship_migrate::ops::status::PlanStatusStepState::Applied,
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
        assert_eq!(
            reply.pending_contracts[0].reason,
            Some(
                zeroship_migrate::PendingContractRefusal::new("widgets", "mig_pending_contract",)
                    .to_string()
            )
        );
        assert_eq!(reply.blocked[0].blocked, blocked_version.as_str());
        assert_eq!(reply.blocked[0].dependency, dependency.as_str());
        assert_eq!(
            reply.blocked[0].reason,
            Some(
                zeroship_migrate::DependencyPendingContract::new(
                    blocked_version.as_str(),
                    dependency.as_str(),
                    "mig_pending_contract",
                )
                .to_string()
            )
        );
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
        assert_eq!(preview_dialect("sqlite"), Ok(zeroship_migrate_sqlite::DIALECT));
        assert!(preview_dialect("oracle").is_err());
    }

    /// The wire spelling a host sends and the id a backend is filed under must be
    /// ONE string. If they ever diverge, `parse` accepts a name the dialect table
    /// does not know, or refuses one it does.
    #[test]
    fn the_host_apply_spelling_is_the_backend_id() {
        assert_eq!(ApplyDialect::Postgres.id(), POSTGRES);
        assert_eq!(ApplyDialect::Mysql.id(), MYSQL);

        for target in [ApplyDialect::Postgres, ApplyDialect::Mysql] {
            let parsed =
                ApplyDialect::parse(target.id().as_str()).expect("a host-apply id round-trips");
            assert_eq!(parsed.id(), target.id());
            assert!(
                shipping_backends().get(&target.id()).is_some(),
                "{} must be a registered backend",
                target.id()
            );
        }

        // A registered backend with no host-driver path and an unregistered name
        // are DIFFERENT refusals, not one catch-all.
        let in_process = ApplyDialect::parse("sqlite").expect_err("sqlite is in-process");
        assert!(in_process.contains("rusqlite"), "{in_process}");
        assert!(!in_process.contains("unknown dialect"), "{in_process}");
        let unregistered = ApplyDialect::parse("duckdb").expect_err("duckdb is not registered");
        assert!(unregistered.contains("unknown dialect"), "{unregistered}");
        assert!(!unregistered.contains("rusqlite"), "{unregistered}");
    }

    /// The parts a caller gets right when it gets nothing wrong: a host-driven
    /// apply with its callback present. Each arm below perturbs ONE field of it, so
    /// a refusal is attributable to that field rather than to the fixture.
    fn host_parts() -> DriverParts<'static> {
        DriverParts {
            kind: HOST_DRIVER_KIND,
            app_path: None,
            migrator_role: None,
            applied_by: Some("host"),
            host_driver_supplied: true,
        }
    }

    /// The same, for the in-process driver.
    fn in_process_parts() -> DriverParts<'static> {
        DriverParts {
            kind: IN_PROCESS_DRIVER_KIND,
            app_path: Some("/tmp/app.db"),
            migrator_role: None,
            applied_by: None,
            host_driver_supplied: false,
        }
    }

    /// The driver and the dialect are separate axes, and BOTH pairings the addon
    /// serves resolve. Without this the refusal arms below would pass over a
    /// decoder that refused everything.
    #[test]
    fn the_served_driver_and_dialect_pairings_resolve() {
        assert_eq!(
            DriverTarget::<RoleAndLabel>::resolve("postgres", &host_parts()),
            Ok(DriverTarget::Host {
                dialect: ApplyDialect::Postgres,
                credentials: RoleAndLabel {
                    migrator_role: None,
                    applied_by: "host".to_string(),
                },
            })
        );
        assert_eq!(
            DriverTarget::<RoleAndLabel>::resolve(
                "mysql",
                &DriverParts {
                    migrator_role: Some("migrator"),
                    ..host_parts()
                }
            ),
            Ok(DriverTarget::Host {
                dialect: ApplyDialect::Mysql,
                credentials: RoleAndLabel {
                    migrator_role: Some("migrator".to_string()),
                    applied_by: "host".to_string(),
                },
            })
        );
        assert_eq!(
            DriverTarget::<RoleAndLabel>::resolve("sqlite", &in_process_parts()),
            Ok(DriverTarget::InProcessSqlite {
                app_path: "/tmp/app.db".to_string(),
            })
        );
    }

    /// A dialect never selects the transport, and a transport never selects the
    /// dialect. Each unserved pairing is refused NAMING the pair, so a caller is
    /// not told to fix the axis it already had right.
    #[test]
    fn an_unserved_driver_and_dialect_pairing_is_refused() {
        // The transport the addon has no in-process backend for.
        let host_only = DriverTarget::<RoleAndLabel>::resolve("postgres", &in_process_parts())
            .expect_err("postgres has no in-process backend here");
        assert!(host_only.contains("serves only the sqlite dialect"), "{host_only}");
        assert!(host_only.contains("postgres"), "{host_only}");

        // And the reverse: the dialect with no JavaScript driver, asked for over one.
        let in_process_only =
            DriverTarget::<RoleAndLabel>::resolve("sqlite", &host_parts()).expect_err("sqlite has no host driver");
        assert!(in_process_only.contains("rusqlite"), "{in_process_only}");

        // A vendor name is not a driver kind. The two axes are spelled from
        // overlapping vocabularies, so this confusion gets its own diagnostic
        // rather than the dialect arm's.
        let vendor_kind = DriverTarget::<RoleAndLabel>::resolve(
            "sqlite",
            &DriverParts {
                kind: "sqlite",
                ..in_process_parts()
            },
        )
        .expect_err("a dialect is not a driver kind");
        assert!(vendor_kind.contains("unknown driver kind"), "{vendor_kind}");
    }

    /// The callback is positional, so nothing in the request alone can catch a
    /// caller that selected one driver and passed the other's argument. Both
    /// directions refuse.
    #[test]
    fn the_driver_kind_and_the_callback_argument_must_agree() {
        let no_callback = DriverTarget::<RoleAndLabel>::resolve(
            "postgres",
            &DriverParts {
                host_driver_supplied: false,
                ..host_parts()
            },
        )
        .expect_err("a host apply with no callback has nothing to drive");
        assert!(
            no_callback.contains("requires a host-driver callback"),
            "{no_callback}"
        );

        let spurious_callback = DriverTarget::<RoleAndLabel>::resolve(
            "sqlite",
            &DriverParts {
                host_driver_supplied: true,
                ..in_process_parts()
            },
        )
        .expect_err("an in-process apply drives no callback");
        assert!(
            spurious_callback.contains("takes no host-driver callback"),
            "{spurious_callback}"
        );
    }

    /// A field belonging to the other driver is REFUSED, never ignored. Silently
    /// dropping `appliedBy` would journal a label the caller believes it set.
    #[test]
    fn each_driver_refuses_the_other_drivers_fields() {
        let host_with_files = DriverTarget::<RoleAndLabel>::resolve(
            "postgres",
            &DriverParts {
                app_path: Some("/tmp/app.db"),
                ..host_parts()
            },
        )
        .expect_err("the host driver opens no files");
        assert!(host_with_files.contains("appPath"), "{host_with_files}");

        let in_process_with_label = DriverTarget::<RoleAndLabel>::resolve(
            "sqlite",
            &DriverParts {
                applied_by: Some("host"),
                ..in_process_parts()
            },
        )
        .expect_err("the in-process deploy loop journals its own label");
        assert!(
            in_process_with_label.contains("appliedBy"),
            "{in_process_with_label}"
        );

        let host_without_label = DriverTarget::<RoleAndLabel>::resolve(
            "postgres",
            &DriverParts {
                applied_by: None,
                ..host_parts()
            },
        )
        .expect_err("the host apply journals the label it is given");
        assert!(
            host_without_label.contains("appliedBy"),
            "{host_without_label}"
        );

        let in_process_missing_files = DriverTarget::<RoleAndLabel>::resolve(
            "sqlite",
            &DriverParts {
                app_path: None,
                ..in_process_parts()
            },
        )
        .expect_err("the in-process driver has nothing to open without appPath");
        assert!(
            in_process_missing_files.contains("appPath"),
            "{in_process_missing_files}"
        );
    }

    /// The transport half of a driver is one question for every verb; the
    /// credential half is not, and this is where each verb's answer is pinned.
    ///
    /// Each arm below runs over the SAME parts as the apply arms above, varying only
    /// the credentials type, so a difference here is attributable to the verb rather
    /// than to a different request.
    #[test]
    fn each_verbs_driver_carries_only_the_credentials_that_verb_journals_under() {
        // A status reconciles. Its driver carries neither field, and setting either
        // is refused rather than dropped: a caller that believes it narrowed the
        // identity a bootstrap runs under would be wrong and never told.
        assert_eq!(
            DriverTarget::<NoCredentials>::resolve(
                "postgres",
                &DriverParts {
                    applied_by: None,
                    ..host_parts()
                }
            ),
            Ok(DriverTarget::Host {
                dialect: ApplyDialect::Postgres,
                credentials: NoCredentials,
            })
        );
        let status_with_label = DriverTarget::<NoCredentials>::resolve("postgres", &host_parts())
            .expect_err("a status records no journal row for a label to name");
        assert!(status_with_label.contains("appliedBy"), "{status_with_label}");
        let status_with_role = DriverTarget::<NoCredentials>::resolve(
            "postgres",
            &DriverParts {
                applied_by: None,
                migrator_role: Some("migrator"),
                ..host_parts()
            },
        )
        .expect_err("a status takes no narrower identity");
        assert!(status_with_role.contains("migratorRole"), "{status_with_role}");

        // A rollback runs reverse DDL, so its host driver may narrow to a role. Its
        // label is not a driver field: BOTH of its drivers journal under the
        // request's own, and a second spelling here would let one driver read it.
        assert_eq!(
            DriverTarget::<RoleOnly>::resolve(
                "mysql",
                &DriverParts {
                    applied_by: None,
                    migrator_role: Some("migrator"),
                    ..host_parts()
                }
            ),
            Ok(DriverTarget::Host {
                dialect: ApplyDialect::Mysql,
                credentials: RoleOnly {
                    migrator_role: Some("migrator".to_string()),
                },
            })
        );
        let rollback_with_label = DriverTarget::<RoleOnly>::resolve("postgres", &host_parts())
            .expect_err("a rollback driver carries no label");
        assert!(rollback_with_label.contains("appliedBy"), "{rollback_with_label}");
        assert!(rollback_with_label.contains("request"), "{rollback_with_label}");

        // The in-process half does NOT vary by verb, and this is the control that
        // says so: the same field is refused identically whichever credentials the
        // verb's host driver admits.
        let in_process_with_role = DriverParts {
            migrator_role: Some("migrator"),
            ..in_process_parts()
        };
        let refusals = [
            DriverTarget::<NoCredentials>::resolve("sqlite", &in_process_with_role).err(),
            DriverTarget::<RoleOnly>::resolve("sqlite", &in_process_with_role).err(),
            DriverTarget::<RoleAndLabel>::resolve("sqlite", &in_process_with_role).err(),
        ];
        for refusal in &refusals {
            let refusal = refusal
                .as_ref()
                .expect("the in-process driver opens the only identity there is");
            assert!(refusal.contains("migratorRole"), "{refusal}");
        }
        assert!(
            refusals.windows(2).all(|pair| pair[0] == pair[1]),
            "one rule, one message: {refusals:?}"
        );
    }

    /// One ordered sequence serves both drivers, and this is the split that makes
    /// that true: the host driver applies the LAST envelope and treats everything
    /// before it as a prefix that must already be journalled.
    #[test]
    fn the_host_split_keeps_the_last_envelope_as_the_one_being_applied() {
        let sequence = ["first", "second", "third"];
        assert_eq!(
            split_host_envelopes(&sequence),
            Ok((&sequence[..2], &sequence[2]))
        );

        let lone = ["only"];
        assert_eq!(split_host_envelopes(&lone), Ok((&lone[..0], &lone[0])));

        // Legal for the in-process deploy loop (a project with no migrations yet),
        // and meaningless here: there is no migration to apply.
        let empty: [&str; 0] = [];
        let refusal = split_host_envelopes(&empty).expect_err("an empty sequence applies nothing");
        assert!(refusal.contains("at least one migration envelope"), "{refusal}");
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
        let version = zeroship_migrate::model::migration::MigrationId::generate();
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
