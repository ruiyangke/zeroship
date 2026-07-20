use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use uuid::Uuid;
use zero_migrate::analysis::analyze::rule::DATA_SECURITY_UNCLASSIFIED_OPS_WARN;
use zero_migrate::apply::journal::DeployRecoveryScope;
use zero_migrate::{
    migrator_role_name, resolve_create_table_policy, snapshot_schema, Approval, ApprovalScope,
    DeclarativeApplyError, EngineError, ExecutorConfig, GuardConfig, IrAuthor,
    LiveSchema, LockMode, MigrationEngine, MigrationIr, PlanStep, PostgresBackend, SealError,
    SealedPolicy, SqlDialect,
};
use zero_migrate_ir::policy_approval::{migration_requires_approval, ApprovalLevel};
use zero_migrate_policy::EffectivePolicy as PdpPolicy;
use zeroship_migrate_adapter::CompioPgSession;

use crate::migration_store::{
    sealed_profile_audit_json, AuditAction, AuditInput, MigrationStore, MigrationStoreError,
    StoreMigrationInput, StoredMigration,
};
use crate::policy::{
    confined_guard_policy_for_schema, CreatorPolicyDraft, EffectivePolicy, ManagedPolicyConfig,
    ManagedPolicyError, SealVerifier,
};
use crate::policy_store::{AppPolicyStore, AppPolicyStoreError};
use crate::provisioning::{provision_migrator, ProvisionRoleError};

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ApplyMigrationsRequest {
    pub kind: ApplyKind,
    pub documents: Vec<IrDocument>,
    #[serde(default)]
    pub policy: Option<PolicyDraftDocument>,
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ApplyKind {
    Ir,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct IrDocument {
    pub filename: String,
    pub body: Value,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PolicyDraftDocument {
    pub filename: String,
    pub body: String,
}

#[derive(Debug, Serialize, PartialEq, Eq)]
pub struct ApplyMigrationsResponse {
    pub migration_id: Uuid,
    pub applied: Vec<String>,
    pub skipped: Vec<String>,
    pub pending_contract: Vec<String>,
}

/// A failure loading/lowering/applying a `.ir.json` bundle through the published
/// `zero-migrate` engine over the [`CompioPgSession`] seam.
///
/// The service owns the file-read + lower + apply loop over the published
/// `zero-migrate` engine, so it owns the error taxonomy for it too. The variants
/// map to distinct HTTP statuses (see [`ir_apply_error_kind`]).
#[derive(Debug, thiserror::Error)]
pub enum IrApplyError {
    /// Reading a `.ir.json` file failed.
    #[error("read IR file ({file}): {message}")]
    Read { file: String, message: String },
    /// Introspecting the live schema failed.
    #[error("read Postgres catalog for live facts: {0}")]
    Snapshot(#[source] zero_migrate::DriftError),
    /// A `.ir.json` failed the fail-closed LOAD GATE or guarded lower.
    #[error("IR load/guarded-lower ({file}): {source}")]
    Ir {
        file: String,
        #[source]
        source: zero_migrate::LoadAndLowerGuardedError,
    },
    /// The engine refused or failed the apply.
    #[error("apply: {0}")]
    Apply(#[from] DeclarativeApplyError),
}

impl From<zero_migrate::LoadAndLowerError> for IrApplyError {
    fn from(_: zero_migrate::LoadAndLowerError) -> Self {
        // The service always lowers via the guarded path; the unguarded error is
        // unreachable here, but keep the conversion total.
        Self::Read {
            file: "<unknown>".to_string(),
            message: "unguarded lower error (unreachable on the service path)".to_string(),
        }
    }
}

/// A failure on the sealed shared-infra apply path.
#[derive(Debug, thiserror::Error)]
pub enum SealedApplyError {
    /// The sealed policy did not authenticate or its binding did not match (the
    /// engine `SealError` is a plain enum without `Display`, so it is rendered via
    /// `Debug`).
    #[error("sealed migration policy refused: {0:?}")]
    Seal(SealError),
    /// The guarded IR apply path failed after seal verification.
    #[error(transparent)]
    Apply(#[from] IrApplyError),
}

impl From<SealError> for SealedApplyError {
    fn from(err: SealError) -> Self {
        Self::Seal(err)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ApplyRequestError {
    #[error("at least one .ir.json document is required")]
    Empty,
    #[error("invalid .ir.json filename {0:?}: filenames must be bare and end with .ir.json")]
    InvalidFilename(String),
    #[error("duplicate .ir.json filename {0:?}")]
    DuplicateFilename(String),
    #[error("document {0:?} must be a JSON object")]
    InvalidDocument(String),
    #[error("create migration temp directory: {0}")]
    TempDir(std::io::Error),
    #[error("write {path}: {source}")]
    Write {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error(transparent)]
    Policy(#[from] ManagedPolicyError),
    #[error(transparent)]
    PolicyStore(#[from] AppPolicyStoreError),
    #[error(transparent)]
    MigrationStore(#[from] MigrationStoreError),
    #[error("encode migration request: {0}")]
    EncodeRequest(String),
    #[error("stored migration request is invalid: {0}")]
    DecodeStoredRequest(String),
    #[error("pending migration not found")]
    PendingMigrationNotFound,
    #[error("migration {migration_id} requires operator approval")]
    ApprovalRequiredPending {
        migration_id: Uuid,
        gated_versions: Vec<String>,
    },
    #[error(
        "ceiling changed since submit for migration {migration_id} (submitted v{submitted_ceiling_version}, current v{current_ceiling_version}): re-submit required"
    )]
    ApprovalStaleCeiling {
        migration_id: Uuid,
        submitted_ceiling_version: u64,
        current_ceiling_version: u64,
    },
    #[error(
        "approval preflight changed for migration {migration_id}: re-submit required"
    )]
    ApprovalPreflightChanged {
        migration_id: Uuid,
        reviewed_gated_versions: Vec<String>,
        current_gated_versions: Vec<String>,
    },
    #[error(
        "approved content drifted for migration {migration_id}: re-review required"
    )]
    ApprovalContentDrift { migration_id: Uuid },
    #[error("stored migration policy has invalid ceiling version {0}")]
    StoredPolicyCeilingVersion(i64),
    #[error("migration database connect: {0}")]
    Connect(compio_postgres::Error),
    #[error("migration schema provision: {0}")]
    ProvisionSchema(compio_postgres::Error),
    #[error("migration role provision: {0}")]
    ProvisionRole(#[from] ProvisionRoleError),
    #[error("runtime app role provision: {0}")]
    ProvisionRuntimeRole(compio_postgres::Error),
    #[error("migration preflight: {0}")]
    Preflight(#[from] IrApplyError),
    #[error("sealed migration apply: {0}")]
    Apply(#[from] SealedApplyError),
}

pub async fn apply_ir_documents(
    provision_dsn: &str,
    tmp_root: &Path,
    app_id: &Uuid,
    request: &ApplyMigrationsRequest,
    policy_config: &ManagedPolicyConfig,
    policy_store: &AppPolicyStore,
    migration_store: &MigrationStore,
    principal_id: Uuid,
) -> Result<ApplyMigrationsResponse, ApplyRequestError> {
    validate_request_shape(request)?;
    let policy = resolve_apply_policy(app_id, request, policy_config, policy_store).await?;
    let migration_id = Uuid::now_v7();
    apply_ir_documents_with_policy(
        provision_dsn,
        tmp_root,
        app_id,
        migration_id,
        request,
        policy,
        policy_config,
        migration_store,
        principal_id,
        ApplyAuthorization::Routine,
    )
    .await
}

pub async fn approve_pending_migration(
    provision_dsn: &str,
    tmp_root: &Path,
    app_id: &Uuid,
    migration_id: Uuid,
    policy_config: &ManagedPolicyConfig,
    policy_store: &AppPolicyStore,
    migration_store: &MigrationStore,
    approver_id: Uuid,
) -> Result<ApplyMigrationsResponse, ApplyRequestError> {
    let Some(stored) = migration_store.get_pending(*app_id, migration_id).await? else {
        return Err(ApplyRequestError::PendingMigrationNotFound);
    };

    let request: ApplyMigrationsRequest = serde_json::from_value(stored.request_body.clone())
        .map_err(|err| ApplyRequestError::DecodeStoredRequest(err.to_string()))?;
    let ceiling_version = u64::try_from(stored.ceiling_version)
        .map_err(|_| ApplyRequestError::StoredPolicyCeilingVersion(stored.ceiling_version))?;
    // Recompose the stored migration's effective policy from its request draft against
    // the current ceiling (the engine `EffectivePolicy` is not serde-stored — it is
    // always re-derived from the draft + ceiling; the audit snapshot lives in the
    // store's JSONB column).
    let stored_policy = resolve_apply_policy(app_id, &request, policy_config, policy_store).await?;
    let current_ceiling = policy_config.current_ceiling_for_app(app_id, None)?;
    if ceiling_version != current_ceiling.ceiling_version {
        let message = format!(
            "ceiling changed since submit (submitted v{}, current v{}): re-submit required",
            ceiling_version, current_ceiling.ceiling_version
        );
        migration_store
            .record_audit(AuditInput {
                app_id: *app_id,
                migration_id,
                principal_id: approver_id,
                migration_versions: &stored.gated_versions,
                action: AuditAction::Approve,
                outcome: "rejected_stale",
                effective_profile: &stored_policy.managed,
                sealed_profile: None,
                ceiling_id: &stored_policy.ceiling_id,
                ceiling_version: stored_policy.ceiling_version,
                detail: json!({
                    "reason": "ceiling_changed_since_submit",
                    "submitted_by": stored.submitted_by,
                    "submitted_ceiling_version": ceiling_version,
                    "current_ceiling_id": current_ceiling.ceiling_id,
                    "current_ceiling_version": current_ceiling.ceiling_version,
                    "re_submit_required": true,
                }),
            })
            .await?;
        mark_migration_failed(migration_store, *app_id, migration_id, &message).await;
        return Err(ApplyRequestError::ApprovalStaleCeiling {
            migration_id,
            submitted_ceiling_version: ceiling_version,
            current_ceiling_version: current_ceiling.ceiling_version,
        });
    }

    let policy = resolve_apply_policy(app_id, &request, policy_config, policy_store).await?;

    apply_ir_documents_with_policy(
        provision_dsn,
        tmp_root,
        app_id,
        migration_id,
        &request,
        policy,
        policy_config,
        migration_store,
        approver_id,
        ApplyAuthorization::OperatorApproved { stored: &stored },
    )
    .await
}

/// The standalone APPROVE transition — the state write the task's `approve(
/// migration_id, operator_id)` names. Operator authorization is the CALLER's job
/// (the control-plane HTTP endpoint / dashboard that invokes this is a FOLLOW-ON,
/// out of scope here); this fn only performs the guarded store write:
/// `pending_approval` → `approved`, stamping `approved_by`, `approved_at`, and
/// `approved_checksum = X` (the content checksum the operator reviewed).
///
/// The apply gate later re-resolves the migration to X' and proceeds only if
/// `X' == approved_checksum`, so passing the exact reviewed checksum here is what
/// closes the approve/apply TOCTOU. A mismatch or a status other than
/// `pending_approval` is a no-op ([`MigrationStoreError::NotPending`]).
pub async fn approve(
    migration_store: &MigrationStore,
    app_id: Uuid,
    migration_id: Uuid,
    operator_id: Uuid,
    reviewed_checksum: &str,
) -> Result<(), ApplyRequestError> {
    migration_store
        .mark_approved(app_id, migration_id, operator_id, reviewed_checksum)
        .await?;
    Ok(())
}

#[derive(Clone, Copy)]
enum ApplyAuthorization<'a> {
    Routine,
    OperatorApproved { stored: &'a StoredMigration },
}

#[allow(clippy::too_many_arguments)]
async fn record_approval_accepted(
    migration_store: &MigrationStore,
    app_id: Uuid,
    migration_id: Uuid,
    approver_id: Uuid,
    stored: &StoredMigration,
    policy: &EffectivePolicy,
    approved_checksum: &str,
) -> Result<(), ApplyRequestError> {
    // APPROVE transition: stamp status=approved + approved_checksum = X (the content
    // checksum the operator is approving). The apply gate below re-resolves to X' and
    // proceeds only if X' == X (the closed approve/apply TOCTOU).
    migration_store
        .mark_approved(app_id, migration_id, approver_id, approved_checksum)
        .await?;
    migration_store
        .record_audit(AuditInput {
            app_id,
            migration_id,
            principal_id: approver_id,
            migration_versions: &stored.gated_versions,
            action: AuditAction::Approve,
            outcome: "approved",
            effective_profile: &policy.managed,
            sealed_profile: None,
            ceiling_id: &policy.ceiling_id,
            ceiling_version: policy.ceiling_version,
            detail: json!({
                "submitted_by": stored.submitted_by,
                "reviewed_gated_versions": stored.gated_versions.clone(),
            }),
        })
        .await?;
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn apply_ir_documents_with_policy(
    provision_dsn: &str,
    tmp_root: &Path,
    app_id: &Uuid,
    migration_id: Uuid,
    request: &ApplyMigrationsRequest,
    policy: EffectivePolicy,
    policy_config: &ManagedPolicyConfig,
    migration_store: &MigrationStore,
    principal_id: Uuid,
    authorization: ApplyAuthorization<'_>,
) -> Result<ApplyMigrationsResponse, ApplyRequestError> {
    match request.kind {
        ApplyKind::Ir => {}
    }
    if request.documents.is_empty() {
        return Err(ApplyRequestError::Empty);
    }

    let dir = write_ir_documents(tmp_root, request)?;
    let request_body = serde_json::to_value(request)
        .map_err(|err| ApplyRequestError::EncodeRequest(err.to_string()))?;
    let schema = app_id.to_string();

    // (a) DRIVER: open a native compio session, wrap it in the adapter's
    // `CompioPgSession`, and drive the published engine over it. Provisioning
    // (schema + role) runs over the SAME raw compio `Client`, borrowed back via
    // `session.client()`.
    let session = CompioPgSession::connect(provision_dsn)
        .await
        .map_err(ApplyRequestError::Connect)?;
    session
        .client()
        .batch_execute(&format!(
            "CREATE SCHEMA IF NOT EXISTS {}",
            quote_ident(&schema)
        ))
        .await
        .map_err(ApplyRequestError::ProvisionSchema)?;
    let role = migrator_role_name(&schema)
        .map_err(|_| ProvisionRoleError::BadRoleName(schema.clone()))?;
    // The executor re-vets rendered SQL at apply time from its own policy, so it must
    // carry the same no-inject confined guard charter as guarded lower. The composed
    // inject-bearing policy remains separate and is passed explicitly to shape
    // resolution and `IrAuthor` below.
    let exec_cfg = ExecutorConfig::new(
        schema.clone(),
        schema.clone(),
        guard_policy_for_managed(&schema),
    )
    .with_migrator_role(role.clone());
    provision_migrator(session.client(), &exec_cfg).await?;
    let backend = PostgresBackend::new_generic(&session);

    let preflight = match authorization {
        ApplyAuthorization::Routine => {
            let report = preflight_ir_documents(&session, &exec_cfg, &schema, dir.path(), &policy)
                .await?;
            let gated_versions = gated_versions_for_policy(&policy, &schema, &report);
            let requires_approval = !gated_versions.is_empty();
            if requires_approval {
                migration_store
                    .insert_pending(StoreMigrationInput {
                        app_id: *app_id,
                        migration_id,
                        principal_id,
                        request_body,
                        effective_profile: &policy.managed,
                        ceiling_id: &policy.ceiling_id,
                        ceiling_version: policy.ceiling_version,
                        gated_versions: &gated_versions,
                    })
                    .await?;
                migration_store
                    .record_audit(AuditInput {
                        app_id: *app_id,
                        migration_id,
                        principal_id,
                        migration_versions: &report.all_versions,
                        action: AuditAction::Submit,
                        outcome: "accepted",
                        effective_profile: &policy.managed,
                        sealed_profile: None,
                        ceiling_id: &policy.ceiling_id,
                        ceiling_version: policy.ceiling_version,
                        detail: json!({ "requires_operator_approval": true }),
                    })
                    .await?;
                migration_store
                    .record_audit(AuditInput {
                        app_id: *app_id,
                        migration_id,
                        principal_id,
                        migration_versions: &gated_versions,
                        action: AuditAction::RejectPending,
                        outcome: "requires_operator_approval",
                        effective_profile: &policy.managed,
                        sealed_profile: None,
                        ceiling_id: &policy.ceiling_id,
                        ceiling_version: policy.ceiling_version,
                        detail: json!({
                            "policy_require_approval": format!("{:?}", policy.approval_level(&schema)),
                            "gated_versions": gated_versions,
                        }),
                    })
                    .await?;
                return Err(ApplyRequestError::ApprovalRequiredPending {
                    migration_id,
                    gated_versions,
                });
            }

            // No approval needed → AUTO-approve (`status = approved`), stamping the
            // resolved content checksum X so the apply gate below re-verifies the same
            // migration it planned (X' == approved_checksum).
            migration_store
                .insert_auto_approved(
                    StoreMigrationInput {
                        app_id: *app_id,
                        migration_id,
                        principal_id,
                        request_body,
                        effective_profile: &policy.managed,
                        ceiling_id: &policy.ceiling_id,
                        ceiling_version: policy.ceiling_version,
                        gated_versions: &[],
                    },
                    &report.content_checksum(),
                )
                .await?;
            migration_store
                .record_audit(AuditInput {
                    app_id: *app_id,
                    migration_id,
                    principal_id,
                    migration_versions: &report.all_versions,
                    action: AuditAction::Submit,
                    outcome: "accepted",
                    effective_profile: &policy.managed,
                    sealed_profile: None,
                    ceiling_id: &policy.ceiling_id,
                    ceiling_version: policy.ceiling_version,
                    detail: json!({ "requires_operator_approval": false }),
                })
                .await?;
            report
        }
        ApplyAuthorization::OperatorApproved { stored } => {
            let report =
                match preflight_ir_documents(&session, &exec_cfg, &schema, dir.path(), &policy)
                    .await
                {
                    Ok(report) => report,
                    Err(err) => {
                        migration_store
                            .record_audit(AuditInput {
                                app_id: *app_id,
                                migration_id,
                                principal_id,
                                migration_versions: &stored.gated_versions,
                                action: AuditAction::Approve,
                                outcome: "rejected_preflight",
                                effective_profile: &policy.managed,
                                sealed_profile: None,
                                ceiling_id: &policy.ceiling_id,
                                ceiling_version: policy.ceiling_version,
                                detail: json!({
                                    "submitted_by": stored.submitted_by,
                                    "reviewed_gated_versions": stored.gated_versions.clone(),
                                    "error": err.to_string(),
                                    "re_submit_required": true,
                                }),
                            })
                            .await?;
                        mark_migration_failed(
                            migration_store,
                            *app_id,
                            migration_id,
                            &format!("approval preflight failed: {err}"),
                        )
                        .await;
                        return Err(err);
                    }
                };
            let current_gated_versions = gated_versions_for_policy(&policy, &schema, &report);
            if !same_versions(&stored.gated_versions, &current_gated_versions) {
                migration_store
                    .record_audit(AuditInput {
                        app_id: *app_id,
                        migration_id,
                        principal_id,
                        migration_versions: &report.all_versions,
                        action: AuditAction::Approve,
                        outcome: "rejected_preflight_changed",
                        effective_profile: &policy.managed,
                        sealed_profile: None,
                        ceiling_id: &policy.ceiling_id,
                        ceiling_version: policy.ceiling_version,
                        detail: json!({
                            "submitted_by": stored.submitted_by,
                            "reviewed_gated_versions": stored.gated_versions.clone(),
                            "current_gated_versions": current_gated_versions.clone(),
                            "re_submit_required": true,
                        }),
                    })
                    .await?;
                mark_migration_failed(
                    migration_store,
                    *app_id,
                    migration_id,
                    "approval preflight classification changed: re-submit required",
                )
                .await;
                return Err(ApplyRequestError::ApprovalPreflightChanged {
                    migration_id,
                    reviewed_gated_versions: stored.gated_versions.clone(),
                    current_gated_versions,
                });
            }
            let current_checksum = report.content_checksum();
            // TOCTOU gate: if this migration carried a prior `approved_checksum` (a
            // standalone `approve()` stamped it) and the re-resolved content checksum X'
            // no longer matches, the content DRIFTED after approval — revert to
            // `pending_approval` (fail-closed) and refuse. When the row is still
            // `pending_approval` with no stamped checksum (the fused approve-then-apply
            // path), there is nothing to drift from; the stamp happens next.
            if let Some(approved) = &stored.approved_checksum {
                if approved != &current_checksum {
                    migration_store
                        .record_audit(AuditInput {
                            app_id: *app_id,
                            migration_id,
                            principal_id,
                            migration_versions: &report.all_versions,
                            action: AuditAction::Approve,
                            outcome: "rejected_content_drift",
                            effective_profile: &policy.managed,
                            sealed_profile: None,
                            ceiling_id: &policy.ceiling_id,
                            ceiling_version: policy.ceiling_version,
                            detail: json!({
                                "submitted_by": stored.submitted_by,
                                "approved_checksum": approved,
                                "current_checksum": current_checksum,
                                "re_review_required": true,
                            }),
                        })
                        .await?;
                    // Drift → back to pending_approval (clears the stale approval).
                    if let Err(err) = migration_store
                        .revert_to_pending(
                            *app_id,
                            migration_id,
                            "approved content drifted since approval: re-review required",
                        )
                        .await
                    {
                        tracing::error!(error = %err, "migrated: revert_to_pending failed");
                    }
                    return Err(ApplyRequestError::ApprovalContentDrift { migration_id });
                }
            }
            record_approval_accepted(
                migration_store,
                *app_id,
                migration_id,
                principal_id,
                stored,
                &policy,
                &current_checksum,
            )
            .await?;
            report
        }
    };

    // The effective policy is the same whether or not the migration needed approval —
    // approval is the separate sealed `safety.require_approval` obligation the host
    // enforces via the state machine, NOT a destructive-posture value the apply
    // projects. Both authorization paths apply under the composed policy verbatim.
    let apply_policy = policy.clone();
    // (d) POLICY: seal the effective policy with the zero-migrate-policy HMAC so the
    // apply carries an authenticated, ceiling-stamped integrity token (the audit
    // records its binding: dialect / matcher version / ceiling version / registry
    // digest). Pre-launch: stored seals don't matter — this seal is minted+verified
    // in-process for tamper-detection. This sealed policy drives managed shape/lower;
    // the rendered-DDL guard is the fixed schema-bound no-inject confined charter.
    let sealed_policy = policy_config.seal_effective_for_app(apply_policy.clone())?;
    let sealed_audit = sealed_profile_audit_json(
        sealed_policy.sealed.dialect(),
        sealed_policy.sealed.matcher_version(),
        sealed_policy.sealed.charter_version(),
        &sealed_policy.sealed.registry_digest(),
    );
    tracing::debug!(
        app_id = %app_id,
        ceiling_id = %sealed_policy.ceiling_id,
        ceiling_version = sealed_policy.ceiling_version,
        approved = matches!(authorization, ApplyAuthorization::OperatorApproved { .. }),
        "migrated: applying IR under sealed managed migration policy"
    );
    let approval = match authorization {
        ApplyAuthorization::Routine => Approval::None,
        ApplyAuthorization::OperatorApproved { .. } => Approval::Approved,
    };
    let applied_by = match authorization {
        ApplyAuthorization::Routine => format!("migrated:{principal_id}"),
        ApplyAuthorization::OperatorApproved { .. } => {
            format!("migrated-approved:{principal_id}")
        }
    };
    provision_runtime_app_role(session.client(), &schema, &role)
        .await
        .map_err(ApplyRequestError::ProvisionRuntimeRole)?;
    let outcome = apply_sealed(
        &session,
        &backend,
        sealed_policy.sealed,
        &sealed_policy.verifier,
        &schema,
        dir.path(),
        &exec_cfg,
        &apply_policy.policy,
        approval,
        &applied_by,
    )
    .await;
    let outcome = match outcome {
        Ok(outcome) => {
            provision_runtime_app_role(session.client(), &schema, &role)
                .await
                .map_err(ApplyRequestError::ProvisionRuntimeRole)?;
            migration_store.mark_applied(*app_id, migration_id).await?;
            let applied = outcome.applied.clone();
            let skipped = outcome.skipped.clone();
            let pending_contract = outcome.pending_contract.clone();
            migration_store
                .record_audit(AuditInput {
                    app_id: *app_id,
                    migration_id,
                    principal_id,
                    migration_versions: &preflight.all_versions,
                    action: AuditAction::Apply,
                    outcome: "applied",
                    effective_profile: &apply_policy.managed,
                    sealed_profile: Some(sealed_audit),
                    ceiling_id: &apply_policy.ceiling_id,
                    ceiling_version: apply_policy.ceiling_version,
                    detail: json!({
                        "approval": match authorization {
                            ApplyAuthorization::Routine => "none",
                            ApplyAuthorization::OperatorApproved { .. } => "operator",
                        },
                        "applied": applied,
                        "skipped": skipped,
                        "pending_contract": pending_contract,
                    }),
                })
                .await?;
            outcome
        }
        Err(err) => {
            record_audit_best_effort(
                migration_store,
                AuditInput {
                    app_id: *app_id,
                    migration_id,
                    principal_id,
                    migration_versions: &preflight.all_versions,
                    action: AuditAction::Apply,
                    outcome: "failed",
                    effective_profile: &apply_policy.managed,
                    sealed_profile: Some(sealed_audit),
                    ceiling_id: &apply_policy.ceiling_id,
                    ceiling_version: apply_policy.ceiling_version,
                    detail: json!({
                        "approval": match authorization {
                            ApplyAuthorization::Routine => "none",
                            ApplyAuthorization::OperatorApproved { .. } => "operator",
                        },
                        "error": err.to_string(),
                    }),
                },
            )
            .await;
            mark_migration_failed(migration_store, *app_id, migration_id, &err.to_string()).await;
            return Err(ApplyRequestError::Apply(err));
        }
    };
    Ok(ApplyMigrationsResponse {
        migration_id,
        applied: outcome.applied,
        skipped: outcome.skipped,
        pending_contract: outcome.pending_contract,
    })
}

/// What a sealed apply produced (the subset of the engine's per-file outcomes the
/// service surfaces + audits).
#[derive(Debug, Clone, Default)]
struct SealedApplyOutcome {
    applied: Vec<String>,
    skipped: Vec<String>,
    pending_contract: Vec<String>,
}

/// Apply a `.ir.json` bundle to Postgres through a sealed shared-infra policy, over
/// the [`CompioPgSession`] seam.
///
/// Verifies the in-process MAC + binding (tamper/staleness fail-closed), then drives
/// the published engine's guarded lower + `apply_plan` per file. Table-shape injection
/// is resolved through the composed engine [`EffectivePolicy`](PdpPolicy) that the
/// seal covers. The rendered-DDL guard uses the separately authored, schema-bound
/// no-inject confined charter so it can vet the managed CREATE TABLE emitted by lower.
/// This is the service-owned reimplementation of the in-tree `apply_sealed` +
/// `apply_bundle_ir_postgres`, since the published engine exports neither.
#[allow(clippy::too_many_arguments)]
async fn apply_sealed(
    session: &CompioPgSession,
    backend: &PostgresBackend<'_, CompioPgSession>,
    sealed: SealedPolicy,
    verifier: &SealVerifier,
    owner_app: &str,
    migrations_dir: &Path,
    exec_cfg: &ExecutorConfig,
    policy: &PdpPolicy,
    approval: Approval,
    applied_by: &str,
) -> Result<SealedApplyOutcome, SealedApplyError> {
    verifier.verify(&sealed, policy)?;
    let guard_cfg = guard_config_for_managed(&exec_cfg.project_schema);
    apply_bundle_ir_postgres(
        session,
        backend,
        &exec_cfg.project_schema,
        owner_app,
        migrations_dir,
        exec_cfg,
        &guard_cfg,
        policy,
        approval,
        applied_by,
    )
    .await
    .map_err(SealedApplyError::Apply)
}

/// Discover `*.ir.json` files in a directory, deterministically ordered by path
/// (the service-owned replacement for the engine's removed `discover_ir_files`).
fn discover_ir_files(migrations_dir: &Path) -> Result<Vec<PathBuf>, IrApplyError> {
    let mut ir_files: Vec<PathBuf> = Vec::new();
    let read = std::fs::read_dir(migrations_dir).map_err(|e| IrApplyError::Read {
        file: migrations_dir.display().to_string(),
        message: e.to_string(),
    })?;
    for entry in read {
        let entry = entry.map_err(|e| IrApplyError::Read {
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

/// Deserialize a `.ir.json` envelope, fold the effective table-shape profile into
/// its `createTable` ops (via the engine's `resolve_create_table_policy`), and
/// re-serialize. The result is the self-contained managed table shape the
/// fail-closed load gate accepts under a `forbid` `author_primary_key` profile.
///
/// A malformed envelope surfaces as an `IrApplyError::Read` for the file (the
/// fail-closed load gate would report the same shape); a policy that cannot
/// resolve the createTable surfaces as an `Ir`-class failure via `Read` text.
fn resolve_shape_bytes(
    raw_bytes: &str,
    policy: &PdpPolicy,
    default_schema: &str,
    file: &str,
) -> Result<String, IrApplyError> {
    let ir: MigrationIr = serde_json::from_str(raw_bytes).map_err(|e| IrApplyError::Read {
        file: file.to_string(),
        message: format!("deserialize IR envelope: {e}"),
    })?;
    let resolved = resolve_create_table_policy(&ir, policy, default_schema).map_err(|e| {
        IrApplyError::Read {
            file: file.to_string(),
            message: format!("resolve table-shape policy: {e}"),
        }
    })?;
    serde_json::to_string(&resolved).map_err(|e| IrApplyError::Read {
        file: file.to_string(),
        message: format!("re-serialize resolved IR envelope: {e}"),
    })
}

/// Live facts the PG `.ir.json` apply loop advances between files (the
/// service-owned analogue of the engine's removed `PostgresIrApplyState`).
struct PostgresIrApplyState {
    registry: BTreeMap<String, String>,
    live_schema: LiveSchema,
}

/// Seed the PG IR apply state from the live project schema (the service-owned
/// analogue of the engine's removed `postgres_ir_apply_state`). Introspects the
/// catalog over the [`SqlSession`] seam via the engine's `snapshot_schema` free fn
/// (the session is the same one the backend borrows).
async fn postgres_ir_apply_state(
    session: &CompioPgSession,
    exec_cfg: &ExecutorConfig,
    owner_app: &str,
) -> Result<PostgresIrApplyState, zero_migrate::DriftError> {
    let live = snapshot_schema(session, &exec_cfg.project_schema).await?;
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
        partitions: live.partitions.clone(),
        table_ownership: live
            .tables
            .keys()
            .map(|t| (t.clone(), owner_app.to_string()))
            .collect(),
        sqlite_schemas: BTreeMap::new(),
        logical_columns: BTreeMap::new(),
    };
    Ok(PostgresIrApplyState {
        registry,
        live_schema,
    })
}

/// Apply all `*.ir.json` files in a directory to Postgres over the seam,
/// holding the project advisory lock once across the whole set (via
/// `LockMode::Acquire` on the first file, `AlreadyHeld` on the rest). The
/// service-owned reimplementation of the engine's removed
/// `apply_bundle_ir_postgres`.
#[allow(clippy::too_many_arguments)]
async fn apply_bundle_ir_postgres(
    session: &CompioPgSession,
    backend: &PostgresBackend<'_, CompioPgSession>,
    project_schema: &str,
    owner_app: &str,
    migrations_dir: &Path,
    exec_cfg: &ExecutorConfig,
    guard_cfg: &GuardConfig,
    policy: &PdpPolicy,
    approval: Approval,
    applied_by: &str,
) -> Result<SealedApplyOutcome, IrApplyError> {
    let ir_files = discover_ir_files(migrations_dir)?;
    if ir_files.is_empty() {
        return Ok(SealedApplyOutcome::default());
    }
    let mut state = postgres_ir_apply_state(session, exec_cfg, owner_app)
        .await
        .map_err(IrApplyError::Snapshot)?;

    let mut outcome = SealedApplyOutcome::default();
    for (index, path) in ir_files.iter().enumerate() {
        // The engine's `apply_plan` acquires the project advisory lock in
        // `LockMode::Acquire`; hold it across the whole file set by acquiring on
        // the first file and reusing it (`AlreadyHeld`) for the rest.
        let lock_mode = if index == 0 {
            LockMode::Acquire
        } else {
            LockMode::AlreadyHeld
        };
        let file_outcome = apply_one_ir_file_postgres(
            backend,
            project_schema,
            owner_app,
            path,
            &mut state,
            exec_cfg,
            guard_cfg,
            policy,
            approval,
            applied_by,
            lock_mode,
        )
        .await?;
        outcome.applied.extend(file_outcome.applied);
        outcome.skipped.extend(file_outcome.skipped);
        outcome.pending_contract.extend(file_outcome.pending_contract);
    }
    Ok(outcome)
}

/// Apply one PG `.ir.json` file: read → fail-closed load + guarded lower
/// (`IrAuthor::load_and_lower_guarded`, Postgres dialect) → engine
/// `apply_plan_with_touched_and_depends_scoped`. Advances `state` with the
/// created tables so a later file in the set sees them.
#[allow(clippy::too_many_arguments)]
async fn apply_one_ir_file_postgres(
    backend: &PostgresBackend<'_, CompioPgSession>,
    project_schema: &str,
    owner_app: &str,
    path: &Path,
    state: &mut PostgresIrApplyState,
    exec_cfg: &ExecutorConfig,
    guard_cfg: &GuardConfig,
    policy: &PdpPolicy,
    approval: Approval,
    applied_by: &str,
    lock_mode: LockMode,
) -> Result<SealedApplyOutcome, IrApplyError> {
    let file = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("<unknown>")
        .to_string();
    let raw_bytes = std::fs::read_to_string(path).map_err(|e| IrApplyError::Read {
        file: file.clone(),
        message: e.to_string(),
    })?;
    // Fold the effective table-shape profile into every `createTable` op (system
    // columns/indexes + resolved primary key) BEFORE the fail-closed load gate,
    // which — under a `forbid` `author_primary_key` profile — REFUSES an
    // unresolved createTable. This is the managed-service analogue of the creator
    // build tool's shape fold, and the same normalisation the Phase-F smoke test
    // proves green. Non-`createTable` ops pass through untouched.
    let bytes = resolve_shape_bytes(&raw_bytes, policy, project_schema, &file)?;

    let mut author = IrAuthor::new(project_schema, owner_app, SqlDialect::Postgres, policy);
    if let Some(scope) = guard_cfg.schema_scope() {
        author = author.with_schema_scope(scope);
    }
    let lowered = author
        .load_and_lower_guarded(
            &bytes,
            owner_app,
            &state.registry,
            &state.live_schema,
            guard_cfg,
        )
        .map_err(|source| IrApplyError::Ir {
            file: file.clone(),
            source,
        })?;

    let created_tables = lowered.created_tables.clone();
    let recovery_scope: Option<&DeployRecoveryScope<'_>> = None;
    let outcome = MigrationEngine::new()
        .apply_plan_with_touched_and_depends_scoped(
            &lowered.plan.steps,
            &lowered.touched_tables,
            &lowered.depends_on,
            approval,
            &ApprovalScope::All,
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

    Ok(SealedApplyOutcome {
        applied: outcome.applied.applied,
        skipped: outcome.applied.skipped,
        pending_contract: outcome
            .pending_contract
            .iter()
            .map(|m| m.version.as_str().to_string())
            .collect(),
    })
}

async fn resolve_apply_policy(
    app_id: &Uuid,
    request: &ApplyMigrationsRequest,
    policy_config: &ManagedPolicyConfig,
    policy_store: &AppPolicyStore,
) -> Result<EffectivePolicy, ApplyRequestError> {
    if let Some(policy) = request.policy.as_ref() {
        let draft = CreatorPolicyDraft {
            filename: policy.filename.as_str(),
            body: policy.body.as_str(),
        };
        let parsed = policy_config.parse_draft(&draft)?;
        return Ok(policy_config.compose_effective_for_app(app_id, None, Some(&parsed))?);
    }

    let Some(stored) = policy_store.get_current(*app_id).await? else {
        return Ok(policy_config.compose_effective_for_app(app_id, None, None)?);
    };
    // Recompose the STORED creator draft (the source of truth, held as `raw_toml`)
    // against the current ceiling. The engine `EffectivePolicy` is not serde-stored;
    // it is always re-derived. If the pinned ceiling version still matches, the
    // recomposition IS the stored effective policy (the compose is deterministic);
    // otherwise the ceiling moved and the fresh composition is authoritative.
    let parsed = stored.parsed_policy_draft()?;
    Ok(policy_config.compose_effective_for_app(app_id, None, Some(&parsed))?)
}

#[derive(Debug, Clone, Default)]
struct PreflightReport {
    all_versions: Vec<String>,
    gated_versions: Vec<String>,
    /// The sorted `version=checksum` fold over every lowered migration — the CONTENT
    /// fingerprint the approval TOCTOU gate pins (`approved_checksum`). Each
    /// per-migration checksum already covers the migration's whole apply-relevant unit
    /// (up/down/flags/deps/preconditions), so any content edit changes this fold even
    /// when version-ids are unchanged.
    checksum_parts: Vec<String>,
}

impl PreflightReport {
    /// The deterministic content checksum of the whole migration set (the sorted
    /// `version=checksum` fold). Empty set → empty string.
    fn content_checksum(&self) -> String {
        let mut parts = self.checksum_parts.clone();
        parts.sort();
        parts.dedup();
        parts.join("\n")
    }
}

async fn preflight_ir_documents(
    session: &CompioPgSession,
    exec_cfg: &ExecutorConfig,
    schema: &str,
    migrations_dir: &Path,
    policy: &EffectivePolicy,
) -> Result<PreflightReport, ApplyRequestError> {
    let files = discover_ir_files(migrations_dir)?;
    let guard_cfg = guard_config_for_managed(schema);
    let mut state = postgres_ir_apply_state(session, exec_cfg, schema)
        .await
        .map_err(IrApplyError::Snapshot)?;
    let mut report = PreflightReport::default();
    let engine = MigrationEngine::new();

    for path in files {
        let file = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("<unknown>")
            .to_string();
        let raw_bytes = std::fs::read_to_string(&path).map_err(|err| IrApplyError::Read {
            file: file.clone(),
            message: err.to_string(),
        })?;
        // Parse the IR envelope up front so we can ask the SEALED approval obligation
        // whether these OPS require approval (`migration_requires_approval`) — the
        // engine only DECLARES the obligation; this host is its enforcer.
        let ir: MigrationIr =
            serde_json::from_str(&raw_bytes).map_err(|e| IrApplyError::Read {
                file: file.clone(),
                message: format!("deserialize IR envelope: {e}"),
            })?;
        let ops_require_approval = migration_requires_approval(&policy.policy, &ir.ops, schema);
        // Fold the effective table-shape profile the same way the apply path does,
        // so preflight lowers the SAME resolved artifact it will apply (identical
        // version-ids + destructive/approval classification).
        let bytes = resolve_shape_bytes(&raw_bytes, &policy.policy, schema, &file)?;
        let mut author = IrAuthor::new(schema, schema, SqlDialect::Postgres, &policy.policy);
        if let Some(scope) = guard_cfg.schema_scope() {
            author = author.with_schema_scope(scope);
        }
        let lowered = author
            .load_and_lower_guarded(
                &bytes,
                schema,
                &state.registry,
                &state.live_schema,
                &guard_cfg,
            )
            .map_err(|source| IrApplyError::Ir {
                file: file.clone(),
                source,
            })?;

        let migrations = lowered.migrations();
        report
            .all_versions
            .extend(migrations.iter().map(|migration| migration.version.as_str().to_string()));
        // Content fingerprint: fold each lowered migration's checksum (which already
        // covers its whole apply-relevant unit) into the set's TOCTOU checksum.
        report.checksum_parts.extend(
            migrations
                .iter()
                .map(|m| format!("{}={}", m.version.as_str(), m.checksum.as_str())),
        );
        // The SEALED obligation applied to THESE ops: if it requires approval, gate
        // every version this file produced (this is the `always`, and the ops-level
        // `on_destructive`, decision — object-scoped + OR-ed across ops by the engine
        // query, independent of the SQL-text destructive classification below).
        if ops_require_approval {
            report.gated_versions.extend(
                migrations
                    .iter()
                    .map(|migration| migration.version.as_str().to_string()),
            );
        }
        let plan = engine.plan(&migrations, &guard_cfg);
        if !plan.denied.is_empty() {
            return Err(IrApplyError::Apply(DeclarativeApplyError::Plain(
                EngineError::Denied(plan.denied),
            ))
            .into());
        }
        for item in plan.items {
            let has_unknown_data_security = item
                .report
                .advisories
                .iter()
                .any(|advisory| advisory.rule == DATA_SECURITY_UNCLASSIFIED_OPS_WARN);
            if item.report.destructive
                || item.migration.flags.requires_approval
                || has_unknown_data_security
            {
                report
                    .gated_versions
                    .push(item.migration.version.as_str().to_string());
            }
        }

        for step in &lowered.plan.steps {
            if let Some(version) = gated_step_version(step) {
                report.gated_versions.push(version);
            }
            if let Some(version) = step_version(step) {
                report.all_versions.push(version);
            }
        }
        for table in lowered.created_tables {
            state
                .registry
                .entry(table.clone())
                .or_insert_with(|| schema.to_string());
            state.live_schema.tables.insert(table);
        }
    }

    dedupe(&mut report.all_versions);
    dedupe(&mut report.gated_versions);
    Ok(report)
}

/// Build the rendered-DDL guard from the authored no-inject confined charter, bound
/// to the same exact app schema as the inject-bearing policy used during lower.
fn guard_config_for_managed(schema: &str) -> GuardConfig {
    GuardConfig::from_policy(guard_policy_for_managed(schema), SqlDialect::Postgres)
}

fn guard_policy_for_managed(schema: &str) -> PdpPolicy {
    confined_guard_policy_for_schema(schema)
        .expect("embedded no-inject confined guard charter must bind and compose")
}

/// The versions of a migration set that require operator approval, folding the SEALED
/// `safety.require_approval` obligation over the engine's own per-migration gating.
///
/// - `report.gated_versions` are the versions the ENGINE already flags (destructive
///   ops, `flags.requires_approval`, unclassified data-security) — these always gate.
/// - The policy obligation LEVEL then widens the set: `always` gates EVERY version
///   (`report.all_versions`); `on_destructive` is already covered (destructive
///   versions are in `report.gated_versions`); `never` adds nothing.
///
/// `schema` is the app's owned schema — the object the object-scoped obligation is
/// resolved at.
fn gated_versions_for_policy(
    policy: &EffectivePolicy,
    schema: &str,
    report: &PreflightReport,
) -> Vec<String> {
    let mut gated = report.gated_versions.clone();
    if policy.approval_level(schema) == ApprovalLevel::Always {
        gated.extend(report.all_versions.iter().cloned());
    }
    dedupe(&mut gated);
    gated
}

fn same_versions(left: &[String], right: &[String]) -> bool {
    left.iter().collect::<BTreeSet<_>>() == right.iter().collect::<BTreeSet<_>>()
}

fn gated_step_version(step: &PlanStep) -> Option<String> {
    if let Some(version) = step.approval_scope_version() {
        return Some(version.to_string());
    }
    match step {
        PlanStep::Ddl(migration) if migration.flags.requires_approval => {
            Some(migration.version.as_str().to_string())
        }
        _ => None,
    }
}

fn step_version(step: &PlanStep) -> Option<String> {
    match step {
        PlanStep::Ddl(migration) => Some(migration.version.as_str().to_string()),
        PlanStep::Dml { version, .. } => Some(version.as_str().to_string()),
        PlanStep::AlterPrimaryKey(_)
        | PlanStep::SynchronizeIdentity(_)
        | PlanStep::OnlineRename(_) => step.approval_scope_version().map(ToOwned::to_owned),
        PlanStep::Backfill { .. } => None,
    }
}

fn dedupe(values: &mut Vec<String>) {
    let mut seen = BTreeSet::new();
    values.retain(|value| seen.insert(value.clone()));
}

fn write_ir_documents(
    tmp_root: &Path,
    request: &ApplyMigrationsRequest,
) -> Result<tempfile::TempDir, ApplyRequestError> {
    validate_request_shape(request)?;
    std::fs::create_dir_all(tmp_root).map_err(ApplyRequestError::TempDir)?;
    let dir = tempfile::Builder::new()
        .prefix("zeroship-migrated-")
        .tempdir_in(tmp_root)
        .map_err(ApplyRequestError::TempDir)?;

    for doc in &request.documents {
        let path = dir.path().join(&doc.filename);
        let bytes = serde_json::to_vec_pretty(&doc.body).map_err(|source| {
            ApplyRequestError::Write {
                path: path.clone(),
                source: std::io::Error::other(source.to_string()),
            }
        })?;
        std::fs::write(&path, bytes).map_err(|source| ApplyRequestError::Write {
            path: path.clone(),
            source,
        })?;
    }
    Ok(dir)
}

fn validate_request_shape(request: &ApplyMigrationsRequest) -> Result<(), ApplyRequestError> {
    match request.kind {
        ApplyKind::Ir => {}
    }
    if request.documents.is_empty() {
        return Err(ApplyRequestError::Empty);
    }
    let mut seen = HashSet::new();
    for doc in &request.documents {
        validate_filename(&doc.filename)?;
        if !seen.insert(doc.filename.clone()) {
            return Err(ApplyRequestError::DuplicateFilename(doc.filename.clone()));
        }
        if !doc.body.is_object() {
            return Err(ApplyRequestError::InvalidDocument(doc.filename.clone()));
        }
    }
    Ok(())
}

async fn record_audit_best_effort(migration_store: &MigrationStore, input: AuditInput<'_>) {
    if let Err(err) = migration_store.record_audit(input).await {
        tracing::error!(error = %err, "migrated: failed to write migration audit row");
    }
}

async fn mark_migration_failed(
    migration_store: &MigrationStore,
    app_id: Uuid,
    migration_id: Uuid,
    message: &str,
) {
    if let Err(store_err) = migration_store
        .mark_rejected(app_id, migration_id, message)
        .await
    {
        tracing::error!(error = %store_err, "migrated: failed to mark migration rejected");
    }
}

fn validate_filename(filename: &str) -> Result<(), ApplyRequestError> {
    let path = Path::new(filename);
    if filename.is_empty()
        || !filename.ends_with(".ir.json")
        || path.components().count() != 1
        || filename.contains('/')
        || filename.contains('\\')
        || filename == "."
        || filename == ".."
    {
        return Err(ApplyRequestError::InvalidFilename(filename.to_owned()));
    }
    Ok(())
}

pub fn apply_error_kind(err: &ApplyRequestError) -> (ntex::http::StatusCode, &'static str) {
    match err {
        ApplyRequestError::Empty
        | ApplyRequestError::InvalidFilename(_)
        | ApplyRequestError::DuplicateFilename(_)
        | ApplyRequestError::InvalidDocument(_) => (
            ntex::http::StatusCode::BAD_REQUEST,
            "invalid_migration_request",
        ),
        ApplyRequestError::Policy(policy) if policy.is_creator_fault() => (
            ntex::http::StatusCode::UNPROCESSABLE_ENTITY,
            "migration_policy_invalid",
        ),
        ApplyRequestError::ApprovalRequiredPending { .. } => (
            ntex::http::StatusCode::CONFLICT,
            "migration_requires_operator_approval",
        ),
        ApplyRequestError::ApprovalStaleCeiling { .. } => (
            ntex::http::StatusCode::CONFLICT,
            "migration_approval_stale_ceiling",
        ),
        ApplyRequestError::ApprovalPreflightChanged { .. } => (
            ntex::http::StatusCode::CONFLICT,
            "migration_approval_preflight_changed",
        ),
        ApplyRequestError::ApprovalContentDrift { .. } => (
            ntex::http::StatusCode::CONFLICT,
            "migration_approval_content_drift",
        ),
        ApplyRequestError::PendingMigrationNotFound | ApplyRequestError::MigrationStore(MigrationStoreError::NotPending) => (
            ntex::http::StatusCode::NOT_FOUND,
            "pending_migration_not_found",
        ),
        ApplyRequestError::Preflight(source) => ir_apply_error_kind(source),
        ApplyRequestError::Apply(source) => sealed_apply_error_kind(source),
        ApplyRequestError::TempDir(_)
        | ApplyRequestError::Write { .. }
        | ApplyRequestError::Policy(_)
        | ApplyRequestError::PolicyStore(_)
        | ApplyRequestError::MigrationStore(_)
        | ApplyRequestError::EncodeRequest(_)
        | ApplyRequestError::DecodeStoredRequest(_)
        | ApplyRequestError::StoredPolicyCeilingVersion(_)
        | ApplyRequestError::Connect(_)
        | ApplyRequestError::ProvisionSchema(_)
        | ApplyRequestError::ProvisionRole(_)
        | ApplyRequestError::ProvisionRuntimeRole(_) => (
            ntex::http::StatusCode::SERVICE_UNAVAILABLE,
            "migration_infrastructure",
        ),
    }
}

fn sealed_apply_error_kind(err: &SealedApplyError) -> (ntex::http::StatusCode, &'static str) {
    match err {
        SealedApplyError::Seal(_) => (
            ntex::http::StatusCode::SERVICE_UNAVAILABLE,
            "migration_policy_seal",
        ),
        SealedApplyError::Apply(source) => ir_apply_error_kind(source),
    }
}

fn ir_apply_error_kind(err: &IrApplyError) -> (ntex::http::StatusCode, &'static str) {
    match err {
        IrApplyError::Ir { .. } | IrApplyError::Apply(_) => (
            ntex::http::StatusCode::UNPROCESSABLE_ENTITY,
            "migration_failed",
        ),
        IrApplyError::Read { .. } | IrApplyError::Snapshot(_) => (
            ntex::http::StatusCode::SERVICE_UNAVAILABLE,
            "migration_infrastructure",
        ),
    }
}

fn quote_ident(ident: &str) -> String {
    format!("\"{}\"", ident.replace('"', "\"\""))
}

fn quote_lit(value: &str) -> String {
    value.replace('\'', "''")
}

const APP_ROLE_TEMPLATE: &str = "__zeroship_app_role_template";

fn runtime_app_role_name(app_id: &str) -> String {
    format!("app_{app_id}_role")
}

async fn provision_runtime_app_role(
    conn: &compio_postgres::Client,
    schema: &str,
    migrator_role: &str,
) -> Result<(), compio_postgres::Error> {
    let schema_q = quote_ident(schema);
    let role = runtime_app_role_name(schema);
    let role_q = quote_ident(&role);
    let template_q = quote_ident(APP_ROLE_TEMPLATE);
    let migrator_q = quote_ident(migrator_role);

    conn.batch_execute(&format!(
        "DO $runtime_app_role$ BEGIN
            IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = '{template_lit}') THEN
                EXECUTE 'CREATE ROLE {template_q} NOLOGIN NOREPLICATION NOCREATEDB NOCREATEROLE NOINHERIT';
            END IF;
            IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = '{role_lit}') THEN
                EXECUTE 'CREATE ROLE {role_q} NOLOGIN NOREPLICATION NOCREATEDB NOCREATEROLE INHERIT IN ROLE {template_q}';
            END IF;
         END $runtime_app_role$",
        template_lit = quote_lit(APP_ROLE_TEMPLATE),
        role_lit = quote_lit(&role),
    ))
    .await?;

    conn.batch_execute(&format!(
        // USAGE only — the runtime role does DML, never DDL. Object creation
        // (tables, sequences) is the migrator role's job; plugin-db's
        // register_model is a no-op on Postgres. Granting CREATE here would let
        // app runtime code author schema objects, which it must not.
        "GRANT USAGE ON SCHEMA {schema_q} TO {role_q};
         GRANT SELECT, INSERT, UPDATE, DELETE ON ALL TABLES IN SCHEMA {schema_q} TO {role_q};
         GRANT USAGE, SELECT ON ALL SEQUENCES IN SCHEMA {schema_q} TO {role_q};
         ALTER DEFAULT PRIVILEGES FOR ROLE {migrator_q} IN SCHEMA {schema_q}
             GRANT SELECT, INSERT, UPDATE, DELETE ON TABLES TO {role_q};
         ALTER DEFAULT PRIVILEGES FOR ROLE {migrator_q} IN SCHEMA {schema_q}
             GRANT USAGE, SELECT ON SEQUENCES TO {role_q};"
    ))
    .await
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn rejects_non_bare_ir_filenames() {
        for bad in ["../x.ir.json", "nested/x.ir.json", "x.sql", "", "."] {
            assert!(validate_filename(bad).is_err(), "{bad} must be rejected");
        }
        assert!(validate_filename("0001_create_notes.ir.json").is_ok());
    }

    #[test]
    fn request_requires_documents() {
        let request = ApplyMigrationsRequest {
            kind: ApplyKind::Ir,
            documents: Vec::new(),
            policy: None,
        };
        let rt = compio::runtime::Runtime::new().expect("runtime");
        let policy_config = ManagedPolicyConfig::default_confined(
            b"migrated apply test seal key 32 bytes min".to_vec(),
            1,
        )
        .expect("policy config");
        let err = rt
            .block_on(apply_ir_documents(
                "postgres://unused",
                Path::new("/tmp"),
                &Uuid::new_v4(),
                &request,
                &policy_config,
                &AppPolicyStore::new("postgres://unused"),
                &MigrationStore::new("postgres://unused"),
                Uuid::new_v4(),
            ))
            .expect_err("empty request rejected before DB connect");
        assert!(matches!(err, ApplyRequestError::Empty));
    }

    #[test]
    fn rejects_scalar_document() {
        let request = ApplyMigrationsRequest {
            kind: ApplyKind::Ir,
            documents: vec![IrDocument {
                filename: "0001_bad.ir.json".to_string(),
                body: json!(true),
            }],
            policy: None,
        };
        let rt = compio::runtime::Runtime::new().expect("runtime");
        let policy_config = ManagedPolicyConfig::default_confined(
            b"migrated apply test seal key 32 bytes min".to_vec(),
            1,
        )
        .expect("policy config");
        let err = rt
            .block_on(apply_ir_documents(
                "postgres://unused",
                Path::new("/tmp"),
                &Uuid::new_v4(),
                &request,
                &policy_config,
                &AppPolicyStore::new("postgres://unused"),
                &MigrationStore::new("postgres://unused"),
                Uuid::new_v4(),
            ))
            .expect_err("scalar request rejected before DB connect");
        assert!(matches!(err, ApplyRequestError::InvalidDocument(_)));
    }
}
