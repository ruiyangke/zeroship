use std::collections::{BTreeSet, HashSet};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use uuid::Uuid;
use zeroship_migrate::{
    apply_sealed, connect, discover_ir_files, migrator_role_name, postgres_ir_apply_state,
    provision_migrator, Approval, ConnectError, DeclarativeApplyError, EngineError,
    ExecutorConfig, GuardConfig, IrAuthor, MigrationEngine, PlanStep, PostgresBackend,
    PostgresIrApplyError, RoleError, SealedApplyError, SqlDialect,
};

use crate::migration_store::{
    sealed_profile_audit_json, AuditAction, AuditInput, MigrationStore, MigrationStoreError,
    StoreMigrationInput, StoredMigration,
};
use crate::policy::{
    CreatorPolicyDraft, EffectivePolicy, ManagedPolicyConfig, ManagedPolicyError,
};
use crate::policy_store::{AppPolicyStore, AppPolicyStoreError};

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
    #[error("stored migration policy has invalid ceiling version {0}")]
    StoredPolicyCeilingVersion(i64),
    #[error("migration database connect: {0}")]
    Connect(#[from] ConnectError),
    #[error("migration schema provision: {0}")]
    ProvisionSchema(compio_postgres::Error),
    #[error("migration role provision: {0}")]
    ProvisionRole(#[from] RoleError),
    #[error("migration preflight: {0}")]
    Preflight(#[from] PostgresIrApplyError),
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
    let stored_policy = EffectivePolicy {
        ceiling_id: stored.ceiling_id.clone(),
        ceiling_version,
        profile: stored.effective_policy_profile()?,
    };
    let current_ceiling = policy_config.current_ceiling_for_app(app_id, None);
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
                effective_profile: &stored_policy.profile,
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
) -> Result<(), ApplyRequestError> {
    migration_store.mark_approved(app_id, migration_id, approver_id).await?;
    migration_store
        .record_audit(AuditInput {
            app_id,
            migration_id,
            principal_id: approver_id,
            migration_versions: &stored.gated_versions,
            action: AuditAction::Approve,
            outcome: "approved",
            effective_profile: &policy.profile,
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
    let conn = connect(provision_dsn).await?;
    conn.batch_execute(&format!(
        "CREATE SCHEMA IF NOT EXISTS {}",
        quote_ident(&schema)
    ))
    .await
    .map_err(ApplyRequestError::ProvisionSchema)?;
    let role = migrator_role_name(&schema)?;
    let exec_cfg =
        ExecutorConfig::new(schema.clone(), schema.clone()).with_migrator_role(role.clone());
    provision_migrator(&conn, &exec_cfg).await?;
    let backend = PostgresBackend::new(&conn);

    let preflight = match authorization {
        ApplyAuthorization::Routine => {
            let report = preflight_ir_documents(&backend, &exec_cfg, &schema, dir.path(), &policy)
                .await?;
            let gated_versions = gated_versions_for_policy(&policy, &report);
            let requires_approval = !gated_versions.is_empty();
            if requires_approval {
                migration_store
                    .insert_pending(StoreMigrationInput {
                        app_id: *app_id,
                        migration_id,
                        principal_id,
                        request_body,
                        effective_profile: &policy.profile,
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
                        effective_profile: &policy.profile,
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
                        effective_profile: &policy.profile,
                        sealed_profile: None,
                        ceiling_id: &policy.ceiling_id,
                        ceiling_version: policy.ceiling_version,
                        detail: json!({
                            "policy_require_approval": policy.requires_operator_approval(),
                            "gated_versions": gated_versions,
                        }),
                    })
                    .await?;
                return Err(ApplyRequestError::ApprovalRequiredPending {
                    migration_id,
                    gated_versions,
                });
            }

            migration_store
                .insert_submitted(StoreMigrationInput {
                    app_id: *app_id,
                    migration_id,
                    principal_id,
                    request_body,
                    effective_profile: &policy.profile,
                    ceiling_id: &policy.ceiling_id,
                    ceiling_version: policy.ceiling_version,
                    gated_versions: &[],
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
                    effective_profile: &policy.profile,
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
                match preflight_ir_documents(&backend, &exec_cfg, &schema, dir.path(), &policy)
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
                                effective_profile: &policy.profile,
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
            let current_gated_versions = gated_versions_for_policy(&policy, &report);
            if !same_versions(&stored.gated_versions, &current_gated_versions) {
                migration_store
                    .record_audit(AuditInput {
                        app_id: *app_id,
                        migration_id,
                        principal_id,
                        migration_versions: &report.all_versions,
                        action: AuditAction::Approve,
                        outcome: "rejected_preflight_changed",
                        effective_profile: &policy.profile,
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
            record_approval_accepted(
                migration_store,
                *app_id,
                migration_id,
                principal_id,
                stored,
                &policy,
            )
            .await?;
            report
        }
    };

    let apply_policy = match authorization {
        ApplyAuthorization::Routine => policy.clone(),
        ApplyAuthorization::OperatorApproved { .. } => policy.project_for_approved_apply(),
    };
    let sealed_policy = policy_config.seal_effective_for_app(app_id, apply_policy.clone())?;
    let sealed_audit = sealed_profile_audit_json(
        sealed_policy.sealed.posture(),
        sealed_policy.sealed.ceiling_version(),
        sealed_policy.sealed.issued_at(),
        sealed_policy.sealed.nonce(),
    );
    tracing::debug!(
        app_id = %app_id,
        ceiling_id = %sealed_policy.ceiling_id,
        ceiling_version = sealed_policy.ceiling_version,
        posture = ?sealed_policy.sealed.posture(),
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
    let outcome = apply_sealed(
        &backend,
        sealed_policy.sealed,
        &sealed_policy.verifier,
        &schema,
        dir.path(),
        &exec_cfg,
        approval,
        &applied_by,
    )
    .await;
    let outcome = match outcome {
        Ok(outcome) => {
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
                    effective_profile: &apply_policy.profile,
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
                    effective_profile: &apply_policy.profile,
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
    let parsed = stored.parsed_policy_profile()?;
    let effective = stored.effective_policy_profile()?;
    let pinned_ceiling_version = u64::try_from(stored.ceiling_version)
        .map_err(|_| ApplyRequestError::StoredPolicyCeilingVersion(stored.ceiling_version))?;
    let ceiling = policy_config.compose_effective_for_app(app_id, None, Some(&parsed))?;
    if pinned_ceiling_version == ceiling.ceiling_version && effective == ceiling.profile {
        return Ok(EffectivePolicy {
            ceiling_id: ceiling.ceiling_id,
            ceiling_version: ceiling.ceiling_version,
            profile: effective,
        });
    }
    Ok(ceiling)
}

#[derive(Debug, Clone, Default)]
struct PreflightReport {
    all_versions: Vec<String>,
    gated_versions: Vec<String>,
}

async fn preflight_ir_documents(
    backend: &PostgresBackend<'_>,
    exec_cfg: &ExecutorConfig,
    schema: &str,
    migrations_dir: &Path,
    policy: &EffectivePolicy,
) -> Result<PreflightReport, ApplyRequestError> {
    let files = discover_ir_files(migrations_dir).map_err(PostgresIrApplyError::from)?;
    let projected = policy.project_for_preflight();
    let guard_cfg = guard_config_for_profile(schema, &projected.profile);
    let mut state = postgres_ir_apply_state(backend, exec_cfg, schema)
        .await
        .map_err(PostgresIrApplyError::Snapshot)?;
    let mut report = PreflightReport::default();
    let engine = MigrationEngine::new();

    for path in files {
        let file = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("<unknown>")
            .to_string();
        let bytes = std::fs::read_to_string(&path).map_err(|err| PostgresIrApplyError::Read {
            file: file.clone(),
            message: err.to_string(),
        })?;
        let mut author = IrAuthor::new(schema, schema, SqlDialect::Postgres);
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
            .map_err(|source| PostgresIrApplyError::Ir {
                file: file.clone(),
                source,
            })?;

        let migrations = lowered.migrations();
        report
            .all_versions
            .extend(migrations.iter().map(|migration| migration.version.as_str().to_string()));
        let plan = engine.plan(&migrations, &guard_cfg);
        if !plan.denied.is_empty() {
            return Err(PostgresIrApplyError::Apply(DeclarativeApplyError::Plain(
                EngineError::Denied(plan.denied),
            ))
            .into());
        }
        for item in plan.items {
            let has_unknown_data_security = item.report.advisories.iter().any(|advisory| {
                advisory.rule
                    == zeroship_migrate::analysis::analyze::rule::DATA_SECURITY_UNCLASSIFIED_OPS_WARN
            });
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

fn guard_config_for_profile(schema: &str, profile: &zeroship_migrate::PolicyProfile) -> GuardConfig {
    GuardConfig::confined(schema.to_string())
        .with_extension_allowlist(profile.capabilities.extensions.clone())
        .with_data_security(
            profile.data_security.require_rls,
            profile.data_security.destructive_ops,
        )
}

fn gated_versions_for_policy(policy: &EffectivePolicy, report: &PreflightReport) -> Vec<String> {
    if policy.requires_operator_approval() && report.gated_versions.is_empty() {
        report.all_versions.clone()
    } else {
        report.gated_versions.clone()
    }
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
        PlanStep::OnlineRename(_) => step.approval_scope_version().map(ToOwned::to_owned),
        PlanStep::Backfill(_) => None,
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
        .mark_failed(app_id, migration_id, message)
        .await
    {
        tracing::error!(error = %store_err, "migrated: failed to mark migration failed");
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
        ApplyRequestError::PendingMigrationNotFound | ApplyRequestError::MigrationStore(MigrationStoreError::NotPending) => (
            ntex::http::StatusCode::NOT_FOUND,
            "pending_migration_not_found",
        ),
        ApplyRequestError::Preflight(source) => postgres_ir_apply_error_kind(source),
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
        | ApplyRequestError::ProvisionRole(_) => (
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
        SealedApplyError::Apply(source) => postgres_ir_apply_error_kind(source),
    }
}

fn postgres_ir_apply_error_kind(
    err: &PostgresIrApplyError,
) -> (ntex::http::StatusCode, &'static str) {
    match err {
        PostgresIrApplyError::Ir { .. }
        | PostgresIrApplyError::Apply(_)
        | PostgresIrApplyError::DuplicateTsVersion { .. } => (
            ntex::http::StatusCode::UNPROCESSABLE_ENTITY,
            "migration_failed",
        ),
        PostgresIrApplyError::Read { .. }
        | PostgresIrApplyError::Snapshot(_)
        | PostgresIrApplyError::Record { .. } => (
            ntex::http::StatusCode::SERVICE_UNAVAILABLE,
            "migration_infrastructure",
        ),
    }
}

fn quote_ident(ident: &str) -> String {
    format!("\"{}\"", ident.replace('"', "\"\""))
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
