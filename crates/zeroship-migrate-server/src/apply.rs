use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;
use zeroship_migrate::apply::journal::DeployRecoveryScope;
use zeroship_migrate::{
    resolve_create_table_policy, Approval, ApprovalScope, DeclarativeApplyError, EngineError,
    ExecutorConfig, GuardConfig, IrAuthor, LiveSchema, LockMode, MigrationEngine, MigrationIr,
    SealError, SealedPolicy,
};
// PG-shaped surfaces live in the vendor crate now: the neutrality refactor moved
// them off the facade, so this PostgreSQL host names PostgreSQL rather than
// reaching for a re-export that deliberately no longer exists.
use zeroship_migrate_postgres::confinement::PostgresConfinementExt;
use zeroship_migrate_postgres::backend::drift_sql::snapshot_schema;
use zeroship_migrate_postgres::role::migrator_role_name;
use zeroship_migrate_postgres::{PostgresBackend, DIALECT as POSTGRES};
use zeroship_migrate_policy::EffectivePolicy as PdpPolicy;
use crate::session::CompioPgSession;

/// The backends this host hands to every engine entry point.
///
/// `zeroship-migrate` is the composition root and the only crate that knows which
/// backends exist; a host takes the set rather than naming vendors itself, which
/// is what let the engine stop naming vendor crates at all. This service only
/// ever targets PostgreSQL, but that is pinned by the `POSTGRES` dialect passed
/// at each call site, NOT by narrowing the set: the engine resolves a backend by
/// `DialectId` out of whatever it was given, so a narrowed set would only change
/// which errors are reachable, not which backend runs.
const VENDORS: zeroship_migrate_backend::registry::VendorSet =
    zeroship_migrate::shipping_vendors();

use crate::policy::{
    confined_guard_policy_for_schema, CreatorPolicyDraft, EffectivePolicy, ManagedPolicyConfig,
    ManagedPolicyError, SealVerifier,
};
use crate::publication::{reconcile_app_publication, PublicationError};
use crate::schema_apply_store::{
    SchemaApplyInput, SchemaApplyStore, SchemaApplyStoreError, TerminalTransition,
};
use crate::provisioning::{
    exec_retry, provision_audit_unmask_table, provision_migrator, ProvisionRoleError,
};

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ApplyMigrationsRequest {
    pub kind: ApplyKind,
    /// sha256 of the `schema.runtime.json` the SAME build emitted from these
    /// documents, lowercase hex.
    ///
    /// This is the deploy precondition's anchor. `genTypesFromMigrations` calls
    /// `genArtifacts` ONCE and writes both `schema.runtime.json` and
    /// `migrations.ir.json` from that single reply, so the hash names the
    /// descriptor that corresponds to exactly this document set. The control
    /// plane later refuses to make a deploy live unless the manifest's
    /// `runtime_descriptor.hash` equals the hash on the app's newest APPLIED
    /// row - which is how a descriptor claiming a column is masked cannot go
    /// live over a database that still holds plaintext there.
    ///
    /// WHAT THIS PROVES IS ORDERING, NOT TRUTH. The value is client-declared: a
    /// creator who hand-edits both generated files can make them agree about a
    /// lie. Closing that needs the server to re-render the descriptor from the
    /// documents it just applied and refuse a mismatch, which is a separate
    /// change and owes a byte-identity gate first.
    pub descriptor_sha256: String,
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
    /// Reading a `.ir.json` file off disk failed - a real I/O fault on OUR side.
    ///
    /// This is infrastructure (503). It must not carry creator-content failures: a
    /// document that does not parse is the creator's to fix, and answering 5xx tells
    /// a retrying client to try again forever. Those go to [`IrApplyError::Malformed`].
    #[error("read IR file ({file}): {message}")]
    Read { file: String, message: String },
    /// A `.ir.json` the creator authored is not a valid IR envelope, or its declared
    /// shape cannot be resolved under the app's policy.
    ///
    /// Creator fault (422), and the file is named so they know which one.
    #[error("malformed IR document ({file}): {message}")]
    Malformed { file: String, message: String },
    /// Introspecting the live schema failed.
    #[error("read Postgres catalog for live facts: {0}")]
    Snapshot(#[source] zeroship_migrate::DriftError),
    /// A `.ir.json` failed the fail-closed LOAD GATE or guarded lower.
    #[error("IR load/guarded-lower ({file}): {source}")]
    Ir {
        file: String,
        #[source]
        source: zeroship_migrate::LoadAndLowerGuardedError,
    },
    /// The engine refused or failed the apply.
    ///
    /// Creator fault (in the common `authorization denied` case, the engine's
    /// own message carries no table/op detail) - the file is named so they know
    /// which document to open, matching [`IrApplyError::Ir`].
    #[error("apply ({file}): {source}")]
    Apply {
        file: String,
        #[source]
        source: DeclarativeApplyError,
    },
}

impl From<zeroship_migrate::LoadAndLowerError> for IrApplyError {
    fn from(_: zeroship_migrate::LoadAndLowerError) -> Self {
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
    #[error(
        "descriptor_sha256 {0:?} is not lowercase sha256 hex: it must be the sha256 of the \
         schema.runtime.json this build emitted"
    )]
    InvalidDescriptorHash(String),
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
    SchemaApplyStore(#[from] SchemaApplyStoreError),
    #[error("encode migration request: {0}")]
    EncodeRequest(String),
    #[error("migration database connect: {0}")]
    Connect(compio_postgres::Error),
    #[error("migration schema provision: {0}")]
    ProvisionSchema(compio_postgres::Error),
    #[error("migration role provision: {0}")]
    ProvisionRole(#[from] ProvisionRoleError),
    #[error("runtime app role provision: {0}")]
    ProvisionRuntimeRole(compio_postgres::Error),
    #[error("unmask audit table provision: {0}")]
    ProvisionAuditUnmask(compio_postgres::Error),
    #[error("app publication provision: {0}")]
    ProvisionPublication(#[from] PublicationError),
    #[error("migration preflight: {0}")]
    Preflight(#[from] IrApplyError),
    #[error("sealed migration apply: {0}")]
    Apply(#[from] SealedApplyError),
}


/// Apply a frozen `.ir.json` bundle into the app's own schema.
///
/// THERE IS ONE PATH. Until 2026-08-28 there were two, and the second was
/// unreachable in a way that read as a feature: a migration whose preflight gated
/// any version was parked as `pending_approval` and could only proceed through an
/// operator `approve()` endpoint that no dashboard, CLI or service ever called. A
/// creator whose migration gated a version therefore got a 409 with no way past
/// it. The approval capability is removed rather than completed; the ceiling the
/// creator runs under is what bounds them, and the engine still refuses a
/// destructive step it was not handed an [`Approval`] for.
///
/// The row in `zeroship.app_schema_applies` is opened BEFORE the engine runs and
/// closed by exactly one of `mark_applied` / `mark_failed`.
pub async fn apply_ir_documents(
    provision_dsn: &str,
    tmp_root: &Path,
    app_id: &Uuid,
    request: &ApplyMigrationsRequest,
    policy_config: &ManagedPolicyConfig,
    schema_apply_store: &SchemaApplyStore,
    principal_id: Uuid,
) -> Result<ApplyMigrationsResponse, ApplyRequestError> {
    validate_request_shape(request)?;
    let policy = resolve_apply_policy(app_id, request, policy_config)?;
    let migration_id = Uuid::now_v7();

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
    let mut exec_cfg = ExecutorConfig::new(
        schema.clone(),
        schema.clone(),
        guard_policy_for_managed(&schema),
    )
    .with_migrator_role(role.clone());
    // THE MIGRATION JOURNAL LIVES IN THE APP'S OWN SCHEMA. `ExecutorConfig::new`
    // derives `meta_schema` as `<project_schema>_migrations`; this host points it at
    // the project schema itself, so the engine writes
    // `"<app_uuid>".__zeroship_schema_migrations` and its five siblings. The record
    // of what ran belongs to the tenant whose schema it describes.
    //
    // THE `__zeroship_` PREFIX IS WHAT MAKES THIS SAFE, and it is not decoration.
    // The engine bootstraps its journal with `CREATE TABLE IF NOT EXISTS` and its
    // table names are literals - only the schema is interpolated - so a creator
    // declaring a table called `schema_migrations` in their own schema would have it
    // silently ADOPTED as the journal. The prefix moves the names into a namespace
    // creator-declared collections are refused from.
    exec_cfg.confinement.meta_schema = schema.clone();
    provision_migrator(session.client(), &exec_cfg).await?;
    // The per-app unmask audit table, which the worker WRITES and creates no
    // longer. Until this change `crud/unmask.rs` emitted its `CREATE TABLE` and
    // three `CREATE INDEX` on every `unmask()` call; that was the last live DDL
    // in the data plane.
    //
    // BEFORE `provision_runtime_app_role` below, and that is not cosmetic. The
    // runtime role's `INSERT` and its `USAGE` on the `BIGSERIAL` sequence both
    // come from `GRANT ... ON ALL TABLES/SEQUENCES IN SCHEMA`, which grants over
    // what exists when it runs. Created after them, the table and its sequence
    // would both be unreachable to the only process that writes to it.
    provision_audit_unmask_table(session.client(), &schema)
        .await
        .map_err(ApplyRequestError::ProvisionAuditUnmask)?;
    let backend = PostgresBackend::new_generic(&session);

    // PREFLIGHT runs before any row is written and before any DDL: it lowers every
    // document under the guard and refuses a denied plan, so a bundle the engine
    // would reject leaves neither a ledger row nor a half-built schema.
    preflight_ir_documents(&session, &exec_cfg, &schema, dir.path(), &policy).await?;

    schema_apply_store
        .record_submitted(SchemaApplyInput {
            app_id: *app_id,
            migration_id,
            principal_id,
            request_body,
            effective_profile: &policy.managed,
            ceiling_id: &policy.ceiling_id,
            ceiling_version: policy.ceiling_version,
            descriptor_sha256: &request.descriptor_sha256,
        })
        .await?;

    let apply_policy = policy.clone();
    // (d) POLICY: seal the effective policy with the zeroship-migrate-policy HMAC so
    // the apply carries an authenticated, ceiling-stamped integrity token.
    // Pre-launch: stored seals don't matter - this seal is minted+verified in-process
    // for tamper-detection. This sealed policy drives managed shape/lower; the
    // rendered-DDL guard is the fixed schema-bound no-inject confined charter.
    let sealed_policy = policy_config.seal_effective_for_app(apply_policy.clone())?;
    tracing::debug!(
        app_id = %app_id,
        ceiling_id = %sealed_policy.ceiling_id,
        ceiling_version = sealed_policy.ceiling_version,
        "migrate-server: applying IR under sealed managed migration policy"
    );
    let applied_by = format!("migrate-server:{principal_id}");
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
        Approval::None,
        &applied_by,
    )
    .await;
    let outcome = match outcome {
        Ok(outcome) => {
            provision_runtime_app_role(session.client(), &schema, &role)
                .await
                .map_err(ApplyRequestError::ProvisionRuntimeRole)?;
            reconcile_app_publication(session.client(), &schema).await?;
            // WRITTEN EVEN WHEN `outcome.applied` IS EMPTY. A re-run that applied
            // nothing is what an app whose descriptor bytes moved without a schema
            // change (an engine upgrade, a codegen fix) uses to become deployable
            // again, because the control plane compares against the NEWEST applied
            // row. Skipping the write for an empty set would brick every such app.
            if schema_apply_store
                .mark_applied(*app_id, migration_id, &outcome.applied)
                .await?
                == TerminalTransition::Lost
            {
                // The DDL is committed and cannot be taken back, but a concurrent
                // path failed this migration while the engine was applying it, so the
                // row reads `failed`. The record now contradicts the database it
                // describes and only an operator can reconcile them. Erroring here
                // would report a failure over changes that did land, so the request
                // continues and the divergence is raised instead.
                tracing::error!(
                    app_id = %app_id,
                    migration_id = %migration_id,
                    "migrate-server: apply lost the terminal transition to a concurrent \
                     failure - schema changes are committed but the row reads failed"
                );
            }
            outcome
        }
        Err(err) => {
            mark_apply_failed(schema_apply_store, *app_id, migration_id, &err.to_string()).await;
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
// `IrApplyError` is ~152 bytes wide because it carries the vendored engine's
// own error enums verbatim (`zeroship_migrate::LoadAndLowerGuardedError` etc).
// Boxing it would ripple through every `IrApplyError::Read { .. }` match arm
// in this file and its callers; not doing that as part of a lint sweep.
#[allow(clippy::result_large_err)]
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
    // Sort on the FILE NAME, which is what the ordering means: migrations apply
    // in filename order, and the leading version is what makes that meaningful.
    //
    // Sorting the full path gives the same answer today only because every entry
    // shares one tempdir parent (`write_ir_documents`). That is true, and it is
    // an accident of the current layout rather than a property of the ordering -
    // a nested or differently-rooted directory would silently sort by prefix and
    // apply migrations out of order, which is the one thing this must not do.
    ir_files.sort_by(|a, b| a.file_name().cmp(&b.file_name()));
    Ok(ir_files)
}

/// Deserialize a `.ir.json` envelope, fold the effective table-shape profile into
/// its `createTable` ops (via the engine's `resolve_create_table_policy`), and
/// re-serialize. The result is the self-contained managed table shape the
/// fail-closed load gate accepts under a `forbid` `author_primary_key` profile.
///
/// A malformed envelope and an unresolvable `createTable` are both the creator's
/// content, so both surface as [`IrApplyError::Malformed`] (422) naming the file. The
/// final re-serialize is of a value this function just built, so a failure there is
/// ours and stays [`IrApplyError::Read`] (503).
// See the `discover_ir_files` allow above - same `IrApplyError` size, same
// rationale.
#[allow(clippy::result_large_err)]
fn resolve_shape_bytes(
    raw_bytes: &str,
    policy: &PdpPolicy,
    default_schema: &str,
    file: &str,
) -> Result<String, IrApplyError> {
    let ir: MigrationIr = serde_json::from_str(raw_bytes).map_err(|e| IrApplyError::Malformed {
        file: file.to_string(),
        message: format!("deserialize IR envelope: {e}"),
    })?;
    let resolved = resolve_create_table_policy(&ir, policy, default_schema).map_err(|e| {
        IrApplyError::Malformed {
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
) -> Result<PostgresIrApplyState, zeroship_migrate::DriftError> {
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
        // EVERY CATALOG FACT THE SNAPSHOT CARRIES IS PASSED THROUGH, not defaulted.
        // These are all `BTreeMap`s of the same snapshot types the introspection
        // returns, so an empty one is not "no opinion" - it is the assertion that
        // the live schema HAS no views / sequences / functions / policies /
        // triggers / extensions, which is what the lowerer would then plan
        // against. `..Default::default()` here would be that assertion made
        // silently, and would go on being made for each field added later.
        views: live.views.clone(),
        sequences: live.sequences.clone(),
        schemas: live.schemas.clone(),
        extensions: live.extensions.clone(),
        functions: live.functions.clone(),
        policies: live.policies.clone(),
        triggers: live.triggers.clone(),
        // The two the catalog genuinely CANNOT answer, left empty on purpose.
        // `sdk_schemas` is a SQLite `renameColumn` rebuild fact and this host is
        // PostgreSQL-only; `logical_columns` and `declared_column_generation` are
        // semantic declarations the ordered-envelope lowerer advances from each
        // artifact, and their own docs say they are never inferred from the
        // physical catalog (a text column cannot reveal that it is a TypeID).
        sdk_schemas: BTreeMap::new(),
        logical_columns: BTreeMap::new(),
        declared_column_generation: BTreeMap::new(),
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

    let mut author = IrAuthor::new(VENDORS, project_schema, owner_app, &POSTGRES, policy);
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
    let outcome = MigrationEngine::new(VENDORS)
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
        .await
        .map_err(|source| IrApplyError::Apply {
            file: file.clone(),
            source,
        })?;

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

/// The policy this apply runs under: the ceiling, narrowed by the draft the
/// REQUEST carried.
///
/// POLICY ARRIVES IN THE ARTIFACT, NOT THE DATABASE. There used to be a
/// `zeroship.migrated_app_policies` table and a PUT endpoint that wrote it, and
/// this function preferred the request draft and fell back to the stored one. The
/// store is gone: a migration policy is declared in the creator's repository,
/// folded at build time and shipped with the request, exactly like the mask
/// policy. A policy nothing can mutate at runtime is one fewer thing to
/// authenticate.
///
/// See the `write_ir_documents` allow below - same `ApplyRequestError` size, same
/// rationale. It surfaces here and not before because this function used to be
/// `async`, and `clippy::result_large_err` does not fire through a future.
#[allow(clippy::result_large_err)]
fn resolve_apply_policy(
    app_id: &Uuid,
    request: &ApplyMigrationsRequest,
    policy_config: &ManagedPolicyConfig,
) -> Result<EffectivePolicy, ApplyRequestError> {
    let Some(policy) = request.policy.as_ref() else {
        return Ok(policy_config.compose_effective_for_app(app_id, None, None)?);
    };
    let draft = CreatorPolicyDraft {
        filename: policy.filename.as_str(),
        body: policy.body.as_str(),
    };
    let parsed = policy_config.parse_draft(&draft)?;
    Ok(policy_config.compose_effective_for_app(app_id, None, Some(&parsed))?)
}

/// Lower every document under the guard and refuse a denied plan, WITHOUT writing
/// anything.
///
/// It used to also classify which versions needed operator approval; that
/// classification had exactly one consumer, an approval state machine no caller
/// could drive, and both are gone. What remains is the reason to run a second
/// lower at all: the real apply lowers per file and executes as it goes, so a
/// bundle whose LAST document is denied would otherwise have already committed the
/// DDL of the ones before it. Preflight walks the whole set first.
///
/// Because it lowers the same artifact the apply will (the same shape fold, the
/// same guard, the same registry threading), a set that passes here and is denied
/// there is a bug rather than a policy decision.
async fn preflight_ir_documents(
    session: &CompioPgSession,
    exec_cfg: &ExecutorConfig,
    schema: &str,
    migrations_dir: &Path,
    policy: &EffectivePolicy,
) -> Result<(), ApplyRequestError> {
    let files = discover_ir_files(migrations_dir)?;
    let guard_cfg = guard_config_for_managed(schema);
    let mut state = postgres_ir_apply_state(session, exec_cfg, schema)
        .await
        .map_err(IrApplyError::Snapshot)?;
    let engine = MigrationEngine::new(VENDORS);

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
        // Fold the effective table-shape profile the same way the apply path does,
        // so preflight lowers the SAME resolved artifact it will apply (identical
        // version-ids + destructive classification).
        let bytes = resolve_shape_bytes(&raw_bytes, &policy.policy, schema, &file)?;
        let mut author = IrAuthor::new(VENDORS, schema, schema, &POSTGRES, &policy.policy);
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

        let plan = engine.plan(&lowered.migrations(), &guard_cfg);
        if !plan.denied.is_empty() {
            return Err(IrApplyError::Apply {
                file: file.clone(),
                source: DeclarativeApplyError::Plain(EngineError::Denied(plan.denied)),
            }
            .into());
        }
        for table in lowered.created_tables {
            state
                .registry
                .entry(table.clone())
                .or_insert_with(|| schema.to_string());
            state.live_schema.tables.insert(table);
        }
    }

    Ok(())
}

/// Build the rendered-DDL guard from the authored no-inject confined charter, bound
/// to the same exact app schema as the inject-bearing policy used during lower.
fn guard_config_for_managed(schema: &str) -> GuardConfig {
    GuardConfig::from_policy(guard_policy_for_managed(schema), POSTGRES)
}

fn guard_policy_for_managed(schema: &str) -> PdpPolicy {
    confined_guard_policy_for_schema(schema)
        .expect("embedded no-inject confined guard charter must bind and compose")
}


// `ApplyRequestError` is ~152 bytes wide because it wraps `IrApplyError` /
// `SealedApplyError` (themselves wide - see the allows on `discover_ir_files`
// above). Boxing it would ripple through every match arm on this type in
// `apply.rs` and `api.rs`; not doing that as part of a lint sweep.
#[allow(clippy::result_large_err)]
fn write_ir_documents(
    tmp_root: &Path,
    request: &ApplyMigrationsRequest,
) -> Result<tempfile::TempDir, ApplyRequestError> {
    validate_request_shape(request)?;
    std::fs::create_dir_all(tmp_root).map_err(ApplyRequestError::TempDir)?;
    let dir = tempfile::Builder::new()
        .prefix("zeroship-migrate-server-")
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

// See the `write_ir_documents` allow above - same `ApplyRequestError` size,
// same rationale.
#[allow(clippy::result_large_err)]
fn validate_request_shape(request: &ApplyMigrationsRequest) -> Result<(), ApplyRequestError> {
    match request.kind {
        ApplyKind::Ir => {}
    }
    if request.documents.is_empty() {
        return Err(ApplyRequestError::Empty);
    }
    if !is_sha256_hex(&request.descriptor_sha256) {
        return Err(ApplyRequestError::InvalidDescriptorHash(
            request.descriptor_sha256.clone(),
        ));
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


/// Close the request's ledger row as `failed`, best effort.
///
/// Best effort because the caller is already returning an error and the apply has
/// already failed: a store outage here must not replace the creator-facing reason
/// with a database one. It is a LOUD best effort - every arm logs, including the
/// lost race, which is the one that means the row and the database disagree.
async fn mark_apply_failed(
    schema_apply_store: &SchemaApplyStore,
    app_id: Uuid,
    migration_id: Uuid,
    message: &str,
) {
    match schema_apply_store
        .mark_failed(app_id, migration_id, message)
        .await
    {
        Ok(TerminalTransition::Recorded) => {}
        Ok(TerminalTransition::Lost) => {
            // The row is already `applied`, so this failure belongs to a migration
            // that another path completed. The failure marking is correctly refused;
            // what would be wrong is letting it pass for a recorded one.
            tracing::warn!(
                app_id = %app_id,
                migration_id = %migration_id,
                reason = message,
                "migrate-server: failure lost the terminal transition to a completed \
                 apply - the row stays applied and this failure is not recorded on it"
            );
        }
        Err(store_err) => {
            tracing::error!(
                error = %store_err,
                "migrate-server: failed to mark the schema apply row failed"
            );
        }
    }
}

// See the `write_ir_documents` allow above - same `ApplyRequestError` size,
// same rationale.
#[allow(clippy::result_large_err)]
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

/// Lowercase sha256 hex, the one spelling the control plane's deploy predicate
/// compares against.
///
/// The comparison is a plain SQL `=` on `text`, so an uppercase or whitespace-
/// padded hash of the SAME bytes would be recorded, would look right in the
/// row, and would refuse every deploy of the app that submitted it. Refusing
/// the spelling at the door is the difference between a 400 naming the field
/// and a 409 nobody can explain.
fn is_sha256_hex(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

pub fn apply_error_kind(err: &ApplyRequestError) -> (ntex::http::StatusCode, &'static str) {
    match err {
        ApplyRequestError::Empty
        | ApplyRequestError::InvalidFilename(_)
        | ApplyRequestError::DuplicateFilename(_)
        | ApplyRequestError::InvalidDescriptorHash(_)
        | ApplyRequestError::InvalidDocument(_) => (
            ntex::http::StatusCode::BAD_REQUEST,
            "invalid_migration_request",
        ),
        ApplyRequestError::Policy(policy) if policy.is_creator_fault() => (
            ntex::http::StatusCode::UNPROCESSABLE_ENTITY,
            "migration_policy_invalid",
        ),
        ApplyRequestError::Preflight(source) => ir_apply_error_kind(source),
        ApplyRequestError::Apply(source) => sealed_apply_error_kind(source),
        ApplyRequestError::TempDir(_)
        | ApplyRequestError::Write { .. }
        | ApplyRequestError::Policy(_)
        | ApplyRequestError::SchemaApplyStore(_)
        | ApplyRequestError::EncodeRequest(_)
        | ApplyRequestError::Connect(_)
        | ApplyRequestError::ProvisionSchema(_)
        | ApplyRequestError::ProvisionRole(_)
        | ApplyRequestError::ProvisionRuntimeRole(_)
        | ApplyRequestError::ProvisionAuditUnmask(_)
        | ApplyRequestError::ProvisionPublication(_) => (
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
        IrApplyError::Ir { .. } | IrApplyError::Apply { .. } => (
            ntex::http::StatusCode::UNPROCESSABLE_ENTITY,
            "migration_failed",
        ),
        // Creator content that never parsed gets its own code, not `migration_failed`:
        // the engine did not refuse this migration, it never saw one.
        IrApplyError::Malformed { .. } => (
            ntex::http::StatusCode::UNPROCESSABLE_ENTITY,
            "migration_invalid",
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
const WORKER_ROLE: &str = "zeroship_worker";

fn runtime_app_role_name(app_id: &str) -> String {
    format!("app_{app_id}_role")
}

/// Both dependents of a freshly provisioned app schema: the worker's membership
/// in the app's runtime role, and the app's workflow journal schema.
///
/// The journal half is [`crate::provisioning::workflow_journal_schema_sql`]
/// verbatim rather than a second copy of it, so a caller that needs a deployed
/// app's journal schema without running an apply
/// ([`crate::provisioning::provision_workflow_journal_schema`]) runs the same
/// statement this does.
fn runtime_dependents_sql(schema: &str, runtime_role: &str) -> String {
    let runtime_role_q = quote_ident(runtime_role);
    let worker_q = quote_ident(WORKER_ROLE);
    format!(
        "DO $runtime_dependents$ BEGIN
            IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = '{worker_lit}') THEN
                GRANT {runtime_role_q} TO {worker_q};
            END IF;
         END $runtime_dependents$;
         {journal}",
        worker_lit = quote_lit(WORKER_ROLE),
        journal = crate::provisioning::workflow_journal_schema_sql(&format!("app_{schema}")),
    )
}

/// PRECONDITION: `schema` and `migrator_role` must contain no single quote.
///
/// The `DO` block below embeds their `quote_ident` forms inside single-quoted
/// `EXECUTE '…'` strings. `quote_ident` doubles internal double-quotes and does
/// nothing to single ones, so a name containing `'` would terminate the EXECUTE
/// literal early and the remainder would be parsed as SQL.
///
/// That is safe today, and only because of where the values come from: `schema`
/// is `app_id.to_string()` (a `Uuid`, so hex and hyphens), and `role` is derived
/// from it as `app_{schema}_role`. Neither can carry a quote. The guarantee lives
/// about a thousand lines from here and nothing at this call site enforced it,
/// which is the whole reason to write it down - a future caller passing a
/// creator-supplied name would find no local objection.
///
/// The durable fix is to stop pre-interpolating and let the block quote its own
/// identifiers with `format('%I', …)`.
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

    exec_retry(conn, &format!(
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

    exec_retry(conn, &format!(
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
    .await?;

    // The migration identity creates no platform role here. It only delegates
    // to the narrow roles that the platform role migration precreated.
    exec_retry(conn, &runtime_dependents_sql(schema, &role)).await
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn runtime_provisioning_delegates_only_precreated_narrow_roles() {
        let sql = runtime_dependents_sql(
            "0191e7a2-b3c4-4d5e-8f90-123456789abc",
            "app_0191e7a2-b3c4-4d5e-8f90-123456789abc_role",
        );
        assert!(sql.contains("IF EXISTS (SELECT 1 FROM pg_roles"));
        assert!(sql.contains(
            "GRANT \"app_0191e7a2-b3c4-4d5e-8f90-123456789abc_role\" TO \"zeroship_worker\""
        ));
        assert!(sql.contains(
            "CREATE SCHEMA IF NOT EXISTS \"app_0191e7a2-b3c4-4d5e-8f90-123456789abc\" AUTHORIZATION \"zeroship_workflow_owner\""
        ));
        assert!(!sql.contains("CREATE ROLE"));
    }

    /// The apply path and the exported
    /// [`provision_workflow_journal_schema`](crate::provisioning::provision_workflow_journal_schema)
    /// build the journal schema DDL from ONE generator, not two that resemble
    /// each other. A caller outside the apply path (control's workflow seeding)
    /// therefore reproduces the deployed privilege shape - owner role, ownership
    /// transfer and grant - rather than a CREATE SCHEMA of its own.
    ///
    /// Asserts the apply path's SQL CONTAINS the exported generator's output
    /// verbatim, so editing either generator alone fails here. It says nothing
    /// about whether the statement is correct, and nothing about whether any
    /// caller actually calls the exported one - the assertions above cover the
    /// shape, and only a live apply covers the effect.
    #[test]
    fn the_apply_path_and_the_exported_helper_share_one_journal_statement() {
        let app_id = uuid::Uuid::parse_str("0191e7a2-b3c4-4d5e-8f90-123456789abc").expect("uuid");
        let schema = app_id.to_string();
        let sql = runtime_dependents_sql(&schema, &runtime_app_role_name(&schema));
        let exported = crate::provisioning::workflow_journal_schema_sql(
            &crate::provisioning::workflow_journal_schema_name(&app_id),
        );
        assert!(
            sql.contains(&exported),
            "apply must embed the exported journal DDL verbatim:\n{sql}\n---\n{exported}"
        );
    }

    /// Every `IrApplyError` a creator can provoke names the document at fault, so
    /// they know which file to open. `Apply` is the variant most likely to carry an
    /// otherwise contentless message - the engine's `authorization denied` names no
    /// table and no operation - which is exactly when the filename is all they get.
    ///
    /// Asserts only that the message carries the filename. It does NOT check the
    /// other variants, and it cannot catch a call site that passes the WRONG file.
    #[test]
    fn an_apply_failure_names_the_document_that_failed() {
        let err = IrApplyError::Apply {
            file: "0007_add_notes.ir.json".to_string(),
            source: DeclarativeApplyError::Plain(EngineError::Denied(Vec::new())),
        };
        let rendered = err.to_string();
        assert!(
            rendered.contains("0007_add_notes.ir.json"),
            "apply failure must name the file; got {rendered:?}"
        );
    }

    /// A syntactically valid descriptor hash for the shape tests, which are
    /// about everything EXCEPT the hash.
    const TEST_DESCRIPTOR_SHA256: &str =
        "1111111111111111111111111111111111111111111111111111111111111111";

    #[test]
    fn descriptor_hash_must_be_lowercase_sha256_hex() {
        let with = |hash: &str| ApplyMigrationsRequest {
            kind: ApplyKind::Ir,
            descriptor_sha256: hash.to_string(),
            documents: vec![IrDocument {
                filename: "0001_notes.ir.json".to_string(),
                body: json!({}),
            }],
            policy: None,
        };
        assert!(validate_request_shape(&with(TEST_DESCRIPTOR_SHA256)).is_ok());
        // The control plane's predicate is a SQL `=` on text, so every spelling
        // below would be stored verbatim and then refuse the app's own deploys.
        for bad in [
            "",
            "1111111111111111111111111111111111111111111111111111111111111",
            "11111111111111111111111111111111111111111111111111111111111111111",
            "1111111111111111111111111111111111111111111111111111111111111111 ",
            "AAAA111111111111111111111111111111111111111111111111111111111111",
            "zzzz111111111111111111111111111111111111111111111111111111111111",
        ] {
            assert!(
                matches!(
                    validate_request_shape(&with(bad)),
                    Err(ApplyRequestError::InvalidDescriptorHash(_))
                ),
                "{bad:?} must be refused as a descriptor hash"
            );
        }
    }

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
            descriptor_sha256: TEST_DESCRIPTOR_SHA256.to_string(),
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
                &SchemaApplyStore::new("postgres://unused"),
                Uuid::new_v4(),
            ))
            .expect_err("empty request rejected before DB connect");
        assert!(matches!(err, ApplyRequestError::Empty));
    }

    #[test]
    fn rejects_scalar_document() {
        let request = ApplyMigrationsRequest {
            kind: ApplyKind::Ir,
            descriptor_sha256: TEST_DESCRIPTOR_SHA256.to_string(),
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
                &SchemaApplyStore::new("postgres://unused"),
                Uuid::new_v4(),
            ))
            .expect_err("scalar request rejected before DB connect");
        assert!(matches!(err, ApplyRequestError::InvalidDocument(_)));
    }
}
