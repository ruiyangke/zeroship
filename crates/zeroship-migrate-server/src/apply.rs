use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;
use zeroship_core::database_role::{per_app_role_name, PerAppRoleNameError};
use zeroship_migrate::apply::journal::DeployRecoveryScope;
use zeroship_migrate::{
    resolve_create_table_policy, Approval, ApprovalScope, DeclarativeApplyError, EngineError,
    ExecutorConfig, GuardConfig, IrAuthor, LiveSchema, LockMode, MigrationBackend, MigrationEngine,
    MigrationIr, SealError, SealedPolicy,
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
    /// Acquiring or releasing the bundle-wide project lock failed.
    #[error("{action} project advisory lock: {source}")]
    ProjectLock {
        action: &'static str,
        #[source]
        source: zeroship_migrate::ApplyError,
    },
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
    ProvisionRuntimeRole(ProvisionRuntimeRoleError),
    #[error("unmask audit table provision: {0}")]
    ProvisionAuditUnmask(compio_postgres::Error),
    #[error("app publication provision: {0}")]
    ProvisionPublication(#[from] PublicationError),
    #[error("migration preflight: {0}")]
    Preflight(#[from] IrApplyError),
    #[error("sealed migration apply: {0}")]
    Apply(#[from] SealedApplyError),
}

/// Failure while deriving or provisioning an app's runtime PostgreSQL role.
#[derive(Debug, thiserror::Error)]
pub enum ProvisionRuntimeRoleError {
    /// The complete authorization-role name exceeds PostgreSQL's identifier
    /// limit and was refused before any SQL was issued.
    #[error(transparent)]
    RoleName(#[from] PerAppRoleNameError),
    /// PostgreSQL refused one of the provisioning statements.
    #[error(transparent)]
    Database(#[from] compio_postgres::Error),
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
/// The row in `zeroship.app_schema_applies` is opened BEFORE the engine runs. A
/// surviving request with a reachable control store closes it through exactly one
/// terminal transition: `mark_applied` on success or `mark_failed` on error. A
/// process death or terminal store failure can leave it `submitted`: the ledger
/// and app schema may live on different DSNs, so no transaction spans them. No
/// serving path reads an open row; the deploy gate reads only `applied` rows.
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

    let result = run_apply(
        &session,
        policy_config,
        &policy,
        &schema,
        dir.path(),
        &exec_cfg,
        &role,
        principal_id,
    )
    .await;

    match result {
        Ok(outcome) => {
            // WRITTEN EVEN WHEN `outcome.applied` IS EMPTY. A re-run that applied
            // nothing is what an app whose descriptor bytes moved without a schema
            // change (an engine upgrade, a codegen fix) uses to become deployable
            // again, because the control plane compares against the NEWEST applied
            // row. Skipping the write for an empty set would brick every such app.
            match schema_apply_store
                .mark_applied(*app_id, migration_id, &outcome.applied)
                .await
            {
                Ok(transition) => {
                    if transition == TerminalTransition::Lost {
                        // The DDL is committed and cannot be taken back, but a
                        // concurrent path failed this migration while the engine was
                        // applying it, so the row reads `failed`. The record now
                        // contradicts the database it describes and only an operator
                        // can reconcile them. Erroring here would report a failure
                        // over changes that did land, so the request continues and
                        // the divergence is raised instead.
                        tracing::error!(
                            app_id = %app_id,
                            migration_id = %migration_id,
                            "migrate-server: apply lost the terminal transition to a concurrent \
                             failure - schema changes are committed but the row reads failed"
                        );
                    }
                    Ok(ApplyMigrationsResponse {
                        migration_id,
                        applied: outcome.applied,
                        skipped: outcome.skipped,
                        pending_contract: outcome.pending_contract,
                    })
                }
                Err(err) => Err(ApplyRequestError::SchemaApplyStore(err)),
            }
        }
        Err(err) => {
            mark_apply_failed(schema_apply_store, *app_id, migration_id, &err.to_string()).await;
            Err(err)
        }
    }
}

/// Run every fallible schema/apply operation after the ledger row opens and before
/// its terminal transition.
///
/// The caller awaits this future without `?`, then routes its one result through
/// the terminal ledger transition. Adding another post-submit operation here
/// cannot create a new unclosed return path in [`apply_ir_documents`].
#[allow(clippy::result_large_err, clippy::too_many_arguments)]
async fn run_apply(
    session: &CompioPgSession,
    policy_config: &ManagedPolicyConfig,
    apply_policy: &EffectivePolicy,
    schema: &str,
    ir_dir: &Path,
    exec_cfg: &ExecutorConfig,
    role: &str,
    principal_id: Uuid,
) -> Result<SealedApplyOutcome, ApplyRequestError> {
    // (d) POLICY: seal the effective policy with the zeroship-migrate-policy HMAC so
    // the apply carries an authenticated, ceiling-stamped integrity token.
    // Pre-launch: stored seals don't matter - this seal is minted+verified in-process
    // for tamper-detection. This sealed policy drives managed shape/lower; the
    // rendered-DDL guard is the fixed schema-bound no-inject confined charter.
    let sealed_policy = policy_config.seal_effective_for_app(apply_policy.clone())?;
    tracing::debug!(
        app_id = %schema,
        ceiling_id = %sealed_policy.ceiling_id,
        ceiling_version = sealed_policy.ceiling_version,
        "migrate-server: applying IR under sealed managed migration policy"
    );
    let applied_by = format!("migrate-server:{principal_id}");
    provision_runtime_app_role(session.client(), schema, role)
        .await
        .map_err(ApplyRequestError::ProvisionRuntimeRole)?;
    let backend = PostgresBackend::new_generic(session);
    let outcome = apply_sealed(
        session,
        &backend,
        sealed_policy.sealed,
        &sealed_policy.verifier,
        schema,
        ir_dir,
        exec_cfg,
        &apply_policy.policy,
        Approval::None,
        &applied_by,
    )
    .await?;
    provision_runtime_app_role(session.client(), schema, role)
        .await
        .map_err(ApplyRequestError::ProvisionRuntimeRole)?;
    reconcile_app_publication(session.client(), schema).await?;
    Ok(outcome)
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
/// acquiring the project advisory lock once on the host's pinned session,
/// passing `LockMode::AlreadyHeld` to every file, and releasing once after the
/// whole set. The service-owned reimplementation of the engine's removed
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

    // `backend` is bound to this host's pinned `session`, so the advisory lock
    // and every file apply run on the same raw PostgreSQL session.
    backend
        .acquire_project_lock(exec_cfg)
        .await
        .map_err(|source| IrApplyError::ProjectLock {
            action: "acquire",
            source,
        })?;

    let body = async {
        let mut state = postgres_ir_apply_state(session, exec_cfg, owner_app)
            .await
            .map_err(IrApplyError::Snapshot)?;
        let mut outcome = SealedApplyOutcome::default();
        for path in &ir_files {
            // The host owns the one outer lock bracket. Every engine plan must
            // leave that lock alone, including the first file.
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
                LockMode::AlreadyHeld,
            )
            .await?;
            outcome.applied.extend(file_outcome.applied);
            outcome.skipped.extend(file_outcome.skipped);
            outcome.pending_contract.extend(file_outcome.pending_contract);
        }
        Ok::<SealedApplyOutcome, IrApplyError>(outcome)
    }
    .await;

    let unlock = backend
        .release_project_lock(exec_cfg)
        .await
        .map_err(|source| IrApplyError::ProjectLock {
            action: "release",
            source,
        });
    match (body, unlock) {
        (Ok(outcome), Ok(())) => Ok(outcome),
        (Ok(_), Err(error)) => Err(error),
        (Err(error), unlock_result) => {
            if let Err(unlock_error) = unlock_result {
                tracing::warn!(
                    error = %unlock_error,
                    project = %exec_cfg.project_id,
                    "migrate-server: failed to release project lock after PG IR apply"
                );
            }
            Err(error)
        }
    }
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
        IrApplyError::Read { .. }
        | IrApplyError::Snapshot(_)
        | IrApplyError::ProjectLock { .. } => (
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
/// The ONE login role every worker process connects as.
///
/// Precreated by the platform migrations
/// (`db/migrations-ts/20260818000200_worker_database_authority.ts`); this
/// service only grants it membership in each app's runtime role.
///
/// Public so end-to-end tests can reach the audit table through the production
/// identity chain (`zeroship_worker` -> `SET ROLE` the app runtime role)
/// without copying this login-role literal.
pub const WORKER_ROLE: &str = "zeroship_worker";

/// Both dependents of a freshly provisioned app schema: the worker's membership
/// in the app's runtime role, and the app's workflow journal schema.
///
/// The journal half is [`crate::provisioning::workflow_journal_schema_sql`]
/// verbatim rather than a second copy of it, so a caller that needs a deployed
/// app's journal schema without running an apply
/// ([`crate::provisioning::provision_workflow_journal_schema`]) runs the same
/// statement this does.
///
/// # `WITH INHERIT FALSE` is what makes `SET LOCAL ROLE` a fence
///
/// [`WORKER_ROLE`] is ONE login role shared by every app. Without the inherit
/// option this grant makes the worker's ambient authority the union of every
/// app runtime role it has ever been granted. The load-bearing fact is the
/// PROPORTION, not the count: on the dev database on 2026-08-28 every single
/// `app_%_role` membership the worker held was `inherit_option = t` - 540 of
/// 540. The count drifts with every test run and is recorded as a dated
/// observation, not a figure to carry forward - and much of that population was
/// this suite's own leak: `cleanup_app` dropped the migrator role but not the
/// `app_<uuid>_role` or its workflow-journal schema, so one green 17-test run
/// left 7 of each behind. Fixed in `9cf1b3fb4`; a later reading of 29 on the
/// same database is the same posture over a smaller population, not a change in
/// it. The worker's boot-time check walks every membership it holds, so the
/// leak was inflating a production-shaped check on every run. Under that posture
/// `SET LOCAL ROLE` only ever NARROWS: a statement that forgets it
/// does not fail, it runs with cross-tenant reach. `WITH INHERIT FALSE` inverts
/// the default so omission fails closed with `permission denied for schema`,
/// and the fence stops depending on every call site remembering.
///
/// Measured on PostgreSQL 16.14 (`server_version_num=160014`), four arms
/// differing in one variable, bare `SELECT` from an app schema as the login
/// role:
///
/// | grant | result |
/// | --- | --- |
/// | `GRANT app TO worker` | row returned - fence open |
/// | same, plus `ALTER ROLE worker NOINHERIT` | row returned - the ATTRIBUTE DOES NOTHING |
/// | `GRANT app TO worker WITH INHERIT FALSE` | `ERROR: permission denied for schema` |
/// | any of the above, plus `SET ROLE app` | row returned - the legitimate path survives |
///
/// The second arm is the trap. PostgreSQL 16+ records `inherit_option` PER
/// MEMBERSHIP at GRANT time, so flipping the role-level attribute does not
/// reach back into a grant that already exists. It has to be on the GRANT.
///
/// # Why no `REVOKE` first, and why that is not an oversight
///
/// This block re-runs on every apply, so existing databases carry the legacy
/// inheriting membership and must converge without a migration. Measured on the
/// same server: issuing `GRANT ... WITH INHERIT FALSE` over an existing
/// inheriting membership flips `pg_auth_members.inherit_option` from `t` to `f`
/// IN PLACE, and the bare `SELECT` that succeeded a statement earlier then
/// fails. A `REVOKE` would be strictly worse - it opens a window in which the
/// worker holds no membership at all, so a concurrent `SET LOCAL ROLE` fails
/// spuriously, and it buys nothing the re-grant does not already do.
///
/// The reverse is a no-op, which is the safe asymmetry: a plain `GRANT` over a
/// non-inheriting membership reports `NOTICE: role ... has already been granted
/// membership` and leaves `inherit_option = f`. A caller that regressed to the
/// old statement could not silently re-open the fence.
///
/// # What this does NOT converge
///
/// `pg_auth_members` is keyed on `(roleid, member, GRANTOR)`. A second grantor
/// adds a SECOND row, PostgreSQL takes the UNION, and one inheriting row
/// re-opens the fence for the pair - measured, including that a plain `REVOKE`
/// as superuser removes only the issuing grantor's row and leaves the other
/// live. This statement converges the row it owns; catching a foreign inheriting
/// row is `zeroship_worker`'s boot-time posture check, which refuses on ANY
/// inheriting app-role membership regardless of who granted it.
fn runtime_dependents_sql_for_role(schema: &str, runtime_role: &str) -> String {
    let runtime_role_q = quote_ident(runtime_role);
    let worker_q = quote_ident(WORKER_ROLE);
    format!(
        "DO $runtime_dependents$ BEGIN
            IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = '{worker_lit}') THEN
                GRANT {runtime_role_q} TO {worker_q} WITH INHERIT FALSE;
            END IF;
         END $runtime_dependents$;
         {journal}",
        worker_lit = quote_lit(WORKER_ROLE),
        journal = crate::provisioning::workflow_journal_schema_sql(&format!("app_{schema}")),
    )
}

/// The complete SQL plan used to provision one app's runtime role.
///
/// The role name is public so the data-plane parity test can compare it as data
/// against its own setup and classifier outputs across the existing
/// dev-dependency edge. The statements remain private to this module and are
/// executed only by the production `provision_runtime_app_role` path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeRoleProvisioningSql {
    role_name: String,
    create_role: String,
    grants: String,
    dependents: String,
}

impl RuntimeRoleProvisioningSql {
    /// The exact, unquoted role identifier carried by every statement.
    #[must_use]
    pub fn role_name(&self) -> &str {
        &self.role_name
    }

    fn statements(&self) -> [&str; 3] {
        [&self.create_role, &self.grants, &self.dependents]
    }
}

/// Build the exact SQL plan used to provision an app's runtime role.
///
/// Public so the data-plane parity test can compare the migration service's
/// production role identifier against its own setup and classifier data.
///
/// # Errors
///
/// Returns an error rather than allowing PostgreSQL to truncate an overlong
/// authorization-role identifier.
///
/// # Preconditions
///
/// `schema` and `migrator_role` must contain no single quote.
///
/// The `DO` block below embeds their `quote_ident` forms inside single-quoted
/// `EXECUTE '...'` strings. `quote_ident` doubles internal double-quotes and does
/// nothing to single ones, so a name containing `'` would terminate the EXECUTE
/// literal early and the remainder would be parsed as SQL.
///
/// That is safe today only because the production caller derives `schema` from
/// a `Uuid`, derives `migrator_role` from that schema, and derives `role_name`
/// through [`per_app_role_name`]. None can carry a quote. The guarantee lives
/// far from this function and nothing here enforces it, so a future caller must
/// preserve the precondition.
///
/// The durable fix is to stop pre-interpolating and let the block quote its own
/// identifiers with `format('%I', ...)`.
pub fn runtime_role_provisioning_sql(
    schema: &str,
    migrator_role: &str,
) -> Result<RuntimeRoleProvisioningSql, PerAppRoleNameError> {
    let schema_q = quote_ident(schema);
    let role_name = per_app_role_name(schema)?;
    let role_q = quote_ident(&role_name);
    let template_q = quote_ident(APP_ROLE_TEMPLATE);
    let migrator_q = quote_ident(migrator_role);

    let create_role = format!(
        "DO $runtime_app_role$ BEGIN
            IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = '{template_lit}') THEN
                EXECUTE 'CREATE ROLE {template_q} NOLOGIN NOREPLICATION NOCREATEDB NOCREATEROLE NOINHERIT';
            END IF;
            IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = '{role_lit}') THEN
                EXECUTE 'CREATE ROLE {role_q} NOLOGIN NOREPLICATION NOCREATEDB NOCREATEROLE INHERIT IN ROLE {template_q}';
            END IF;
         END $runtime_app_role$",
        template_lit = quote_lit(APP_ROLE_TEMPLATE),
        role_lit = quote_lit(&role_name),
    );

    let grants = format!(
        // USAGE only - the runtime role does DML, never DDL. Object creation
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
    );

    // The migration identity creates no platform role here. It only delegates
    // to the narrow roles that the platform role migration precreated.
    let dependents = runtime_dependents_sql_for_role(schema, &role_name);
    Ok(RuntimeRoleProvisioningSql {
        role_name,
        create_role,
        grants,
        dependents,
    })
}

async fn provision_runtime_app_role(
    conn: &compio_postgres::Client,
    schema: &str,
    migrator_role: &str,
) -> Result<(), ProvisionRuntimeRoleError> {
    let provisioning = runtime_role_provisioning_sql(schema, migrator_role)?;
    for statement in provisioning.statements() {
        exec_retry(conn, statement).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn runtime_provisioning_delegates_only_precreated_narrow_roles() {
        let provisioning = runtime_role_provisioning_sql(
            "0191e7a2-b3c4-4d5e-8f90-123456789abc",
            "zs_migrator_fixture",
        )
        .expect("test runtime role name");
        let sql = provisioning.dependents;
        assert!(sql.contains("IF EXISTS (SELECT 1 FROM pg_roles"));
        // The inherit option is part of the ASSERTED string, not a suffix the
        // match tolerates. The previous form stopped at `TO "zeroship_worker"`
        // and so passed identically with and without the fence - the one shape
        // a regression guard for the fence must not have.
        assert!(sql.contains(
            "GRANT \"app_0191e7a2-b3c4-4d5e-8f90-123456789abc_role\" \
             TO \"zeroship_worker\" WITH INHERIT FALSE"
        ));
        // Belt and braces against a re-grant that drops the option: a bare
        // `... TO "zeroship_worker";` terminator is the pre-fix statement.
        assert!(
            !sql.contains("TO \"zeroship_worker\";"),
            "an unqualified grant re-opens the ambient union across every app: {sql}"
        );
        assert!(sql.contains(
            "CREATE SCHEMA IF NOT EXISTS \"app_0191e7a2-b3c4-4d5e-8f90-123456789abc\" AUTHORIZATION \"zeroship_workflow_owner\""
        ));
        assert!(!sql.contains("CREATE ROLE"));
    }

    #[test]
    fn runtime_provisioning_plan_refuses_overlong_role_names() {
        let app_id = "a".repeat(55);
        assert_eq!(
            runtime_role_provisioning_sql(&app_id, "zs_migrator_fixture"),
            Err(PerAppRoleNameError::TooLong {
                actual_bytes: 64,
                max_bytes: 63,
            })
        );
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
        let sql = runtime_role_provisioning_sql(&schema, "zs_migrator_fixture")
            .expect("test runtime role name")
            .dependents;
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

// ---------------------------------------------------------------------------
// Live provisioning proofs for the unmask audit table
// ---------------------------------------------------------------------------
//
// Every case here opens a real PostgreSQL connection and creates roles, so the
// module carries the same `live-db-tests` gate `schema_apply_store`'s tests do:
// `required-features` in Cargo.toml cannot reach inside a lib target, and a
// `cargo test --workspace` that provisions no database must not be made red by
// a case that needs one.
//
// WHAT THESE COVER THAT THE UNIT TESTS ABOVE CANNOT. `audit_unmask_tests` in
// `provisioning.rs` greps the generated string: it proves the DDL SAYS
// `CREATE TABLE IF NOT EXISTS "app"."__zeroship_audit_unmask"`, never that a
// server accepted it, that the table came out with the columns the data plane
// binds, or that the runtime role can reach it afterwards. The whole point of
// moving this DDL out of the worker is a privilege change, and a privilege
// change is only observable against a live catalog.
#[cfg(all(test, feature = "live-db-tests"))]
mod live_audit_unmask_provisioning {
    use super::*;
    use compio_postgres::NoTls;

    /// The database these cases dial. The DSN is typed config
    /// (`zeroship_core::config::test_database_url`, backed by the overlay at
    /// `deploy/ops/zeroship.test.toml` or the pre-existing `PG_TEST_URL`
    /// override) - this module introduces no environment variable of its own
    /// and sets none.
    fn test_dsn() -> String {
        zeroship_core::config::test_database_url()
    }

    async fn admin_client() -> compio_postgres::Client {
        let (client, conn) = compio_postgres::connect(&test_dsn(), NoTls)
            .await
            .expect("connect to the migrate-server test database");
        compio::runtime::spawn(async move {
            let _ = conn.run().await;
        })
        .detach();
        client
    }

    /// A scratch app schema whose name is a real `Uuid`, because
    /// `provision_migrator` derives the migrator role from it and production
    /// only ever passes an app id.
    fn scratch_schema() -> String {
        Uuid::new_v4().to_string()
    }

    fn audit_table_ref(schema: &str) -> String {
        format!("{}.\"__zeroship_audit_unmask\"", quote_ident(schema))
    }

    /// Drop everything a case created, by name. Never a blanket sweep: this
    /// server is shared with other work.
    async fn teardown(admin: &compio_postgres::Client, schema: &str) {
        let _ = admin
            .batch_execute(&format!(
                "DROP SCHEMA IF EXISTS {} CASCADE; DROP SCHEMA IF EXISTS {} CASCADE;",
                quote_ident(schema),
                quote_ident(&format!("app_{schema}")),
            ))
            .await;
        for role in [
            per_app_role_name(schema).expect("scratch runtime role name"),
            migrator_role_name(schema).unwrap_or_default(),
        ] {
            if role.is_empty() {
                continue;
            }
            let q = quote_ident(&role);
            let _ = admin.batch_execute(&format!("DROP OWNED BY {q} CASCADE")).await;
            let _ = admin.batch_execute(&format!("DROP ROLE IF EXISTS {q}")).await;
        }
    }

    /// The production prelude of `apply_ir_request`: `CREATE SCHEMA`, then
    /// `provision_migrator`. Returns the migrator role name, which is what the
    /// apply path passes on to `provision_runtime_app_role`.
    async fn provision_schema_and_migrator(
        admin: &compio_postgres::Client,
        schema: &str,
    ) -> String {
        admin
            .batch_execute(&format!(
                "CREATE SCHEMA IF NOT EXISTS {}",
                quote_ident(schema)
            ))
            .await
            .expect("create scratch app schema");
        let role = migrator_role_name(schema).expect("derive migrator role");
        let mut cfg = ExecutorConfig::new(
            schema.to_string(),
            schema.to_string(),
            guard_policy_for_managed(schema),
        )
        .with_migrator_role(role.clone());
        cfg.confinement.meta_schema = schema.to_string();
        provision_migrator(admin, &cfg)
            .await
            .expect("provision migrator role");
        role
    }

    async fn probe(admin: &compio_postgres::Client, sql: &str) -> bool {
        admin
            .query_one_scalar::<bool, _>(sql, &[])
            .await
            .unwrap_or_else(|e| panic!("probe failed: {sql}: {e}"))
    }

    /// 1. THE TABLE IS ACTUALLY CREATED, with the shape the data plane binds.
    ///
    /// `crud/unmask.rs` INSERTs eight columns by name (`actor_id, actor_role,
    /// collection, row_pk, "column", classification, reason, outcome`) and
    /// relies on `id` and `ts` defaulting. This case reads the catalog for all
    /// eleven, for the `BIGSERIAL`-implied sequence, for the three indexes, and
    /// for the `outcome` CHECK - then re-runs the provisioning, which is what
    /// makes it safe on every apply.
    #[compio::test]
    async fn provisioning_creates_the_audit_table_with_its_columns_and_indexes() {
        let admin = admin_client().await;
        let schema = scratch_schema();
        teardown(&admin, &schema).await;
        provision_schema_and_migrator(&admin, &schema).await;

        provision_audit_unmask_table(&admin, &schema)
            .await
            .expect("provision the audit table");

        let columns: Vec<(String, String, bool)> = admin
            .query(
                "SELECT column_name, data_type, is_nullable = 'YES' \
                   FROM information_schema.columns \
                  WHERE table_schema = $1 AND table_name = '__zeroship_audit_unmask' \
                  ORDER BY ordinal_position",
                &[&schema],
            )
            .await
            .expect("read columns")
            .iter()
            .map(|r| (r.get(0), r.get(1), r.get(2)))
            .collect();
        let names: Vec<&str> = columns.iter().map(|c| c.0.as_str()).collect();
        assert_eq!(
            names,
            [
                "id",
                "ts",
                "actor_id",
                "actor_role",
                "collection",
                "row_pk",
                "column",
                "classification",
                "reason",
                "request_id",
                "outcome",
            ],
            "the audit table's columns, in order: {columns:?}"
        );
        // The five the data plane always supplies must be NOT NULL; the four it
        // may omit must accept a NULL rather than refuse the row.
        for (name, nullable) in [
            ("collection", false),
            ("row_pk", false),
            ("column", false),
            ("classification", false),
            ("outcome", false),
            ("actor_id", true),
            ("actor_role", true),
            ("reason", true),
            ("request_id", true),
        ] {
            let got = columns
                .iter()
                .find(|c| c.0 == name)
                .unwrap_or_else(|| panic!("column {name} missing: {columns:?}"));
            assert_eq!(got.2, nullable, "nullability of {name}: {got:?}");
        }
        assert_eq!(
            columns.iter().find(|c| c.0 == "id").map(|c| c.1.as_str()),
            Some("bigint"),
            "id must be the BIGSERIAL pk: {columns:?}"
        );

        // The BIGSERIAL sequence. Its absence is the second half of the ordering
        // hazard documented on `audit_unmask_table_sql`: a grant reaching the
        // table but not the sequence would still fail every INSERT.
        assert!(
            probe(
                &admin,
                &format!(
                    "SELECT pg_get_serial_sequence('{}', 'id') IS NOT NULL",
                    audit_table_ref(&schema)
                ),
            )
            .await,
            "the id column must own an implicit sequence"
        );

        // OWNERSHIP, which is the change this commit made: the table is created
        // by the migration service's admin principal, so the worker's runtime
        // role is an ordinary grantee rather than the owner it used to be.
        let owner: String = admin
            .query_one_scalar(
                "SELECT tableowner FROM pg_tables \
                  WHERE schemaname = $1 AND tablename = '__zeroship_audit_unmask'",
                &[&schema],
            )
            .await
            .expect("read table owner");
        assert_ne!(
            owner,
            per_app_role_name(&schema).expect("scratch runtime role name"),
            "the runtime role must NOT own its own audit log"
        );

        let indexes: Vec<String> = admin
            .query(
                "SELECT indexname FROM pg_indexes \
                  WHERE schemaname = $1 AND tablename = '__zeroship_audit_unmask' \
                  ORDER BY indexname",
                &[&schema],
            )
            .await
            .expect("read indexes")
            .iter()
            .map(|r| r.get::<_, String>(0))
            .collect();
        assert_eq!(
            indexes,
            [
                "__zeroship_audit_unmask_actor_idx",
                "__zeroship_audit_unmask_pkey",
                "__zeroship_audit_unmask_row_idx",
                "__zeroship_audit_unmask_ts_idx",
            ],
            "three declared indexes plus the primary key: {indexes:?}"
        );

        // The outcome CHECK is the only thing stopping a third outcome string
        // from being recorded, so the server has to be the one enforcing it.
        let bad = admin
            .execute(
                &format!(
                    "INSERT INTO {} (collection, row_pk, \"column\", classification, outcome) \
                     VALUES ('users', '1', 'ssn', 'pii', 'maybe')",
                    audit_table_ref(&schema)
                ),
                &[],
            )
            .await
            .expect_err("outcome CHECK must refuse an unknown outcome");
        assert_eq!(
            bad.code(),
            Some(&compio_postgres::error::SqlState::CHECK_VIOLATION),
            "expected a check violation, got {bad}"
        );

        // Idempotent: this runs on EVERY apply.
        provision_audit_unmask_table(&admin, &schema)
            .await
            .expect("re-running the provisioning must be a no-op");

        teardown(&admin, &schema).await;
    }

    /// 2. THE ORDERING MATTERS, shown by running it the other way round rather
    /// than asserted.
    ///
    /// `provision_runtime_app_role` grants the runtime role its DML with
    /// `GRANT ... ON ALL TABLES IN SCHEMA` / `ON ALL SEQUENCES IN SCHEMA`, which
    /// are snapshots over what exists at that instant, plus `ALTER DEFAULT
    /// PRIVILEGES FOR ROLE <migrator>` for the future - and the audit table is
    /// created by the ADMIN principal, not the migrator, so the default
    /// privileges do not cover it either. Every arm runs exactly the same
    /// production functions against equivalent scratch schemas; only their order
    /// differs.
    ///
    /// ARM C locates the boundary rather than assuming it. `apply_ir_request`
    /// calls `provision_runtime_app_role` TWICE - once before `apply_sealed` and
    /// once after - so the binding constraint is "before the LAST call", not
    /// "before the first". A table created between the two is still reached,
    /// because the second call re-runs the snapshot grants.
    #[compio::test]
    async fn the_audit_table_must_be_provisioned_before_the_runtime_role() {
        let admin = admin_client().await;

        // ARM A - production order: audit table, then runtime role.
        let before = scratch_schema();
        teardown(&admin, &before).await;
        let migrator_before = provision_schema_and_migrator(&admin, &before).await;
        provision_audit_unmask_table(&admin, &before)
            .await
            .expect("provision audit table (production order)");
        provision_runtime_app_role(&admin, &before, &migrator_before)
            .await
            .expect("provision runtime role (production order)");

        // ARM B - the inversion, differing in exactly one variable.
        let after = scratch_schema();
        teardown(&admin, &after).await;
        let migrator_after = provision_schema_and_migrator(&admin, &after).await;
        provision_runtime_app_role(&admin, &after, &migrator_after)
            .await
            .expect("provision runtime role (inverted order)");
        provision_audit_unmask_table(&admin, &after)
            .await
            .expect("provision audit table (inverted order)");

        // ARM C - between the apply path's TWO `provision_runtime_app_role`
        // calls. The second one re-snapshots, so this arm is expected to be
        // REACHABLE, and that is the honest statement of where the boundary is.
        let between = scratch_schema();
        teardown(&admin, &between).await;
        let migrator_between = provision_schema_and_migrator(&admin, &between).await;
        provision_runtime_app_role(&admin, &between, &migrator_between)
            .await
            .expect("provision runtime role (first apply-path call)");
        provision_audit_unmask_table(&admin, &between)
            .await
            .expect("provision audit table (between the two calls)");
        provision_runtime_app_role(&admin, &between, &migrator_between)
            .await
            .expect("provision runtime role (second apply-path call)");

        let insert_priv = |schema: &str| {
            format!(
                "SELECT has_table_privilege('{role}', '{table}', 'INSERT')",
                role = per_app_role_name(schema).expect("scratch runtime role name"),
                table = audit_table_ref(schema),
            )
        };
        let sequence_priv = |schema: &str| {
            format!(
                "SELECT has_sequence_privilege('{role}', \
                   pg_get_serial_sequence('{table}', 'id'), 'USAGE')",
                role = per_app_role_name(schema).expect("scratch runtime role name"),
                table = audit_table_ref(schema),
            )
        };

        assert!(
            probe(&admin, &insert_priv(&before)).await,
            "production order must leave the runtime role able to INSERT"
        );
        assert!(
            probe(&admin, &sequence_priv(&before)).await,
            "production order must leave the runtime role able to use the id sequence"
        );
        // THE CONTROL. Same two calls, opposite order, and the snapshot grants
        // now miss both objects. If this arm ever went true, the ordering
        // comment on `audit_unmask_table_sql` would be describing nothing and
        // arm A would be passing for some other reason.
        assert!(
            !probe(&admin, &insert_priv(&after)).await,
            "a table created AFTER the snapshot grants must NOT be reachable - \
             if it is, the ordering constraint is not what makes the production \
             order work and arm A proves nothing"
        );
        assert!(
            !probe(&admin, &sequence_priv(&after)).await,
            "the implicit sequence must be missed too - it is the second object \
             the ordering hazard costs"
        );
        // ARM C. `audit_unmask_table_sql`'s docstring says "BEFORE
        // `provision_runtime_app_role`"; this is what that actually buys, and it
        // is looser than the sentence reads. Written as an assertion so a change
        // that removed the apply path's SECOND call - making the strict reading
        // true - fails here and forces the docstring to be re-read.
        assert!(
            probe(&admin, &insert_priv(&between)).await,
            "the apply path calls provision_runtime_app_role twice; a table \
             created between them is re-snapshotted by the second call. If this \
             is false, one of those two calls is gone and the ordering docstring \
             on audit_unmask_table_sql needs re-reading"
        );
        assert!(
            probe(&admin, &sequence_priv(&between)).await,
            "the second call must re-snapshot the sequence as well"
        );

        teardown(&admin, &before).await;
        teardown(&admin, &after).await;
        teardown(&admin, &between).await;
    }
}

// ---------------------------------------------------------------------------
// Live proof that `SET LOCAL ROLE` is a FENCE and not an optional narrowing
// ---------------------------------------------------------------------------
//
// `WORKER_ROLE` is ONE login role shared by every app, and `runtime_dependents_sql`
// grants it each app's runtime role. Whether that grant inherits decides whether
// a query path that forgets `SET LOCAL ROLE` fails or silently runs with the
// union of every tenant the worker has ever served. That is a property of the
// SERVER's membership catalog, so only a live catalog can rule on it.
//
// WHAT THE UNIT TEST ABOVE CANNOT DO.
// `runtime_provisioning_delegates_only_precreated_narrow_roles` greps the
// generated string for `WITH INHERIT FALSE`. It proves the statement SAYS the
// words - never that PostgreSQL honours them, never that the role-level
// `NOINHERIT` attribute would not have done the same job, and never that an
// existing database provisioned the old way converges when the statement runs
// again. Every one of those was measured here and two of them are counter-
// intuitive.
//
// THE FIXTURE IS `SET SESSION AUTHORIZATION`, not a second connection. It sets
// both session and current user, so privilege checks run fully as the stand-in
// worker, and it needs no password and no DSN surgery. It requires the
// connecting user to be a superuser; if it is not, these cases fail naming it
// rather than skipping.
#[cfg(all(test, feature = "live-db-tests"))]
mod live_worker_role_fence {
    use super::*;
    use compio_postgres::NoTls;

    /// The database these cases dial. Typed config
    /// (`zeroship_core::config::test_database_url`, backed by the overlay at
    /// `deploy/ops/zeroship.test.toml` or the pre-existing `PG_TEST_URL`
    /// override) - this module introduces no environment variable and sets none.
    fn test_dsn() -> String {
        zeroship_core::config::test_database_url()
    }

    async fn admin_client() -> compio_postgres::Client {
        let (client, conn) = compio_postgres::connect(&test_dsn(), NoTls)
            .await
            .expect("connect to the migrate-server test database");
        compio::runtime::spawn(async move {
            let _ = conn.run().await;
        })
        .detach();
        client
    }

    /// Every object a case creates carries this prefix, so teardown can name
    /// what it drops on a server shared with other work.
    const PREFIX: &str = "zsfence";

    struct Fixture {
        /// The per-case component of every name, which is also what [`sweep`]
        /// matches on. Distinct per case so two cases running on the same
        /// server in parallel - the cargo default - cannot sweep each other.
        case: String,
        app_role: String,
        worker: String,
        schema: String,
    }

    impl Fixture {
        fn new(case: &str) -> Self {
            // Hyphen-free so every name below stays a bare identifier, and short
            // enough that `app_<schema>` (the journal schema the production
            // statement also creates) clears PostgreSQL's 63-byte identifier
            // limit - a truncated name would collide across cases rather than
            // fail.
            let unique = Uuid::new_v4().to_string().replace('-', "");
            Self {
                case: case.to_string(),
                app_role: format!("{PREFIX}_{case}_{unique}_role"),
                worker: format!("{PREFIX}_{case}_{unique}_worker"),
                schema: format!("{PREFIX}_{case}_{unique}_ns"),
            }
        }

        /// The `LIKE` pattern covering every object this case can create.
        fn like(&self) -> String {
            format!("{PREFIX}_{}_%", self.case)
        }
    }

    /// Drop every object THIS CASE has ever created, matched on its own
    /// `zsfence_<case>_` prefix.
    ///
    /// This is a sweep, and the sibling module's "never a blanket sweep" rule
    /// still holds: the pattern reaches only names this case mints, so it
    /// cannot touch other work on a shared server - nor a SIBLING case, which
    /// is why it is scoped per case rather than to [`PREFIX`]. `cargo test`
    /// runs these in parallel by default, and a module-wide sweep would delete
    /// a concurrently-running case's roles out from under it, producing a
    /// failure that looks like a privilege bug.
    ///
    /// It exists because a case that PANICS never reaches its `teardown`, and
    /// an assertion firing is the EXPECTED outcome of three of these five
    /// before the fix - one such run left four schemas and thirteen roles
    /// behind. Sweeping at the head makes that self-correcting.
    ///
    /// Residual, stated rather than hidden: two runs of the SAME case against
    /// the same server (two worktrees, one Postgres) still collide. Nothing
    /// short of a per-run database fixes that, and the sibling module carries
    /// the same exposure.
    async fn sweep(admin: &compio_postgres::Client, fx: &Fixture) {
        let _ = admin.batch_execute("RESET SESSION AUTHORIZATION").await;
        admin
            .batch_execute(&format!(
                "DO $sweep$
                 DECLARE target text;
                 BEGIN
                   FOR target IN
                     SELECT nspname FROM pg_namespace
                      WHERE nspname LIKE '{like}' OR nspname LIKE 'app\\_{like}'
                   LOOP
                     EXECUTE format('DROP SCHEMA IF EXISTS %I CASCADE', target);
                   END LOOP;
                   FOR target IN
                     SELECT rolname FROM pg_roles WHERE rolname LIKE '{like}'
                   LOOP
                     EXECUTE format('DROP OWNED BY %I CASCADE', target);
                     EXECUTE format('DROP ROLE IF EXISTS %I', target);
                   END LOOP;
                 END
                 $sweep$;",
                like = fx.like(),
            ))
            .await
            .expect("sweep this case's leftovers");
    }

    /// Stand up a tenant schema holding one secret, an app runtime role that can
    /// read it, and a stand-in worker login holding NO privilege of its own. The
    /// only thing that can put the secret within the worker's ambient reach is
    /// the membership grant under test.
    async fn setup(admin: &compio_postgres::Client, fx: &Fixture) {
        let (role_q, worker_q, schema_q) = (
            quote_ident(&fx.app_role),
            quote_ident(&fx.worker),
            quote_ident(&fx.schema),
        );
        admin
            .batch_execute(&format!(
                "CREATE ROLE {role_q} NOLOGIN;
                 CREATE ROLE {worker_q} LOGIN;
                 CREATE SCHEMA {schema_q};
                 CREATE TABLE {schema_q}.secrets(v text);
                 INSERT INTO {schema_q}.secrets VALUES ('tenant-secret');
                 GRANT USAGE ON SCHEMA {schema_q} TO {role_q};
                 GRANT SELECT ON {schema_q}.secrets TO {role_q};"
            ))
            .await
            .expect("stand up the tenant schema, app role and stand-in worker");
    }

    /// Drop everything a case created, by name. Never a blanket sweep: this
    /// server is shared with other work.
    ///
    /// `app_<schema>` is the workflow journal schema the SECOND half of
    /// `runtime_dependents_sql` creates. Leaving it behind would accumulate a
    /// schema per run on a shared server, and it is easy to miss because
    /// nothing in these cases mentions the journal.
    async fn teardown(admin: &compio_postgres::Client, fx: &Fixture) {
        let _ = admin.batch_execute("RESET SESSION AUTHORIZATION").await;
        for schema in [fx.schema.clone(), format!("app_{}", fx.schema)] {
            let _ = admin
                .batch_execute(&format!(
                    "DROP SCHEMA IF EXISTS {} CASCADE",
                    quote_ident(&schema)
                ))
                .await;
        }
        for role in [&fx.worker, &fx.app_role] {
            let q = quote_ident(role);
            let _ = admin.batch_execute(&format!("DROP OWNED BY {q} CASCADE")).await;
            let _ = admin.batch_execute(&format!("DROP ROLE IF EXISTS {q}")).await;
        }
    }

    /// THE PRODUCTION STATEMENT, retargeted at the stand-in worker.
    ///
    /// Built by substituting the stand-in's name into
    /// [`runtime_dependents_sql`]'s own output rather than by restating the
    /// grant, so a revert of the fix reaches these cases. The `DO` block's
    /// guard is `IF EXISTS (SELECT 1 FROM pg_roles WHERE rolname = '<worker>')`,
    /// so the substitution has to reach the quoted identifier AND the string
    /// literal or the block silently no-ops and every assertion below would
    /// then be measuring the ABSENCE of a grant while reading as a fence.
    fn production_grant_for(fx: &Fixture) -> String {
        let sql = runtime_dependents_sql_for_role(&fx.schema, &fx.app_role);
        assert!(
            sql.matches(WORKER_ROLE).count() >= 2,
            "expected the worker role as both a quoted ident and a literal: {sql}"
        );
        let retargeted = sql.replace(WORKER_ROLE, &fx.worker);
        // `zeroship_worker` is not a substring of `zeroship_workflow_owner`
        // ("worker" vs "workflow"), so the journal half of the statement must
        // come through untouched. Asserted rather than assumed: a rename that
        // made one a prefix of the other would corrupt the journal DDL silently,
        // and these cases would then be measuring a statement production never
        // issues.
        assert!(
            retargeted.contains("zeroship_workflow_owner"),
            "the substitution must not touch the journal owner: {retargeted}"
        );
        retargeted
    }

    /// Read `pg_auth_members.inherit_option` for one (granted role, member)
    /// pair. Returns every row, because the catalog is unique on
    /// `(roleid, member, GRANTOR)` and a second grantor's inheriting row would
    /// re-open the fence for the pair - a query shaped `LIMIT 1` would report a
    /// fence that a sibling row has already opened.
    async fn inherit_options(
        admin: &compio_postgres::Client,
        fx: &Fixture,
    ) -> Vec<bool> {
        admin
            .query(
                "SELECT membership.inherit_option \
                   FROM pg_auth_members membership \
                   JOIN pg_roles granted ON granted.oid = membership.roleid \
                   JOIN pg_roles member ON member.oid = membership.member \
                  WHERE granted.rolname = $1 AND member.rolname = $2 \
                  ORDER BY membership.grantor",
                &[&fx.app_role, &fx.worker],
            )
            .await
            .expect("read pg_auth_members")
            .iter()
            .map(|row| row.get::<_, bool>(0))
            .collect()
    }

    /// Run SQL as the stand-in worker; `Ok(value)` is the first column of the
    /// first row returned, `Err(message)` a refusal.
    ///
    /// The **simple** protocol, because [`fenced_select`] is a multi-statement
    /// batch and the extended protocol refuses those with `42601 cannot insert
    /// multiple commands into a prepared statement` - a failure that is not a
    /// privilege refusal and would be miscounted as one by a laxer check.
    ///
    /// That is why the error arm asserts the SQLSTATE is exactly 42501 rather
    /// than string-matching "denied": a fixture typo, a missing table or the
    /// wrong protocol all produce an `Err`, and every one of them would read as
    /// "the fence held".
    ///
    /// `RESET SESSION AUTHORIZATION` runs on BOTH arms before returning, so a
    /// refusal cannot leave the shared admin connection impersonating the
    /// stand-in for the rest of the case - which would make every later
    /// statement, including teardown, fail for the wrong reason.
    async fn denied_as_worker(
        admin: &compio_postgres::Client,
        fx: &Fixture,
        sql: &str,
    ) -> Result<String, String> {
        admin
            .batch_execute(&format!(
                "SET SESSION AUTHORIZATION {}",
                quote_ident(&fx.worker)
            ))
            .await
            .expect(
                "SET SESSION AUTHORIZATION needs a superuser connection; \
                 point the test overlay at one",
            );
        let outcome = admin.simple_query(sql).await;
        admin
            .batch_execute("RESET SESSION AUTHORIZATION")
            .await
            .expect("restore the admin identity");

        match outcome {
            Ok(messages) => Ok(messages
                .iter()
                .find_map(|message| match message {
                    compio_postgres::SimpleQueryMessage::Row(row) => {
                        Some(row.get(0).unwrap_or_default().to_string())
                    }
                    _ => None,
                })
                .unwrap_or_default()),
            Err(error) => {
                // `Error: Display` renders the KIND ("db error"), not the
                // server's message - so a caller that string-matched
                // `to_string()` for "permission denied" would fail on a refusal
                // that is exactly right. The server text lives on the nested
                // `DbError`, and the SQLSTATE is what the case actually binds.
                let db_error = error
                    .as_db_error()
                    .unwrap_or_else(|| panic!("expected a server error, got {error}"));
                assert_eq!(
                    db_error.code(),
                    &compio_postgres::error::SqlState::INSUFFICIENT_PRIVILEGE,
                    "expected a privilege refusal (42501), got {} {}",
                    db_error.code().code(),
                    db_error.message()
                );
                Err(db_error.message().to_string())
            }
        }
    }

    fn bare_select(fx: &Fixture) -> String {
        format!("SELECT v FROM {}.secrets", quote_ident(&fx.schema))
    }

    /// The legitimate path, in the shape the data plane uses it:
    /// `SET LOCAL` inside an explicit transaction, so the role reverts at
    /// COMMIT (`auth::bootstrap::autocommit_local_session_setup_sql`). Outside
    /// a transaction `SET LOCAL` is a no-op that only WARNS, so a variant
    /// without the `BEGIN` would be measuring the bare read under a different
    /// name.
    fn fenced_select(fx: &Fixture) -> String {
        format!(
            "BEGIN; SET LOCAL ROLE {}; SELECT v FROM {}.secrets; COMMIT",
            quote_ident(&fx.app_role),
            quote_ident(&fx.schema)
        )
    }

    /// 1. THE REGRESSION. The production grant must leave the shared worker
    /// login UNABLE to read a tenant schema without `SET LOCAL ROLE`.
    ///
    /// FAILS BEFORE THE FIX. With the pre-fix `GRANT <role> TO <worker>` the
    /// bare SELECT returns `tenant-scoped-secret` and the `expect_err` below is
    /// the assertion that goes red.
    #[compio::test]
    async fn the_production_grant_denies_a_bare_read_of_a_tenant_schema() {
        let admin = admin_client().await;
        let fx = Fixture::new("prod");
        sweep(&admin, &fx).await;
        setup(&admin, &fx).await;

        admin
            .batch_execute(&production_grant_for(&fx))
            .await
            .expect("run the production runtime-dependents statement");

        // THE BEHAVIOUR FIRST, deliberately. `pg_auth_members` is the mechanism;
        // being refused the row is the property, and it is the property that
        // should name itself when this goes red. Asserting the catalog first
        // would make a pre-fix run report `left: [true], right: [false]` - true
        // but a description of a bit, not of a tenant boundary.
        let denial = denied_as_worker(&admin, &fx, &bare_select(&fx))
            .await
            .expect_err(
                "the shared worker login must NOT reach a tenant schema without \
                 SET LOCAL ROLE - if this returns the row, the fence is open and \
                 every query path that forgets to narrow reads across tenants",
            );
        assert!(
            denial.contains("permission denied"),
            "unexpected denial: {denial}"
        );

        // Then the catalog, which says WHY it was refused. Without this a
        // revocation of the app role's own grants would also produce a denial
        // and read as a working fence.
        let options = inherit_options(&admin, &fx).await;
        assert_eq!(
            options.len(),
            1,
            "expected exactly one membership row: {options:?}"
        );
        assert!(
            options.iter().all(|inherits| !inherits),
            "the production grant must record inherit_option = false: {options:?}"
        );

        teardown(&admin, &fx).await;
    }

    /// 2. THE FENCE MUST NOT BREAK THE LEGITIMATE PATH. Same grant, same
    /// connection, one extra statement.
    #[compio::test]
    async fn set_local_role_still_reaches_the_tenant_schema_under_the_fence() {
        let admin = admin_client().await;
        let fx = Fixture::new("setlocal");
        sweep(&admin, &fx).await;
        setup(&admin, &fx).await;

        admin
            .batch_execute(&production_grant_for(&fx))
            .await
            .expect("run the production runtime-dependents statement");

        let value = denied_as_worker(&admin, &fx, &fenced_select(&fx))
            .await
            .expect("SET LOCAL ROLE must still reach the tenant schema");
        assert_eq!(
            value, "tenant-secret",
            "the fenced path must return the row"
        );

        teardown(&admin, &fx).await;
    }

    /// 3. THE CONTROL, differing in ONE variable: the inherit option.
    ///
    /// Without this, case 1 would pass just as happily if the app role had
    /// never been granted USAGE on the schema - "denied" is the default state
    /// of a role with no privileges, and a fixture that mis-provisioned would
    /// read as a working fence. This arm runs the SHIPPED pre-fix statement and
    /// requires the bare read to SUCCEED, which is simultaneously the proof
    /// that the fixture grants something real and the demonstration of the
    /// vulnerability.
    ///
    /// The second arm is the trap the role attribute sets: PostgreSQL 16+
    /// records `inherit_option` per membership at GRANT time, so
    /// `ALTER ROLE <worker> NOINHERIT` does NOT reach back into a grant that
    /// already exists and the bare read still succeeds. Anyone reaching for the
    /// attribute instead of the grant option gets a green that means nothing.
    #[compio::test]
    async fn the_legacy_grant_leaks_and_the_role_attribute_does_not_stop_it() {
        let admin = admin_client().await;
        let fx = Fixture::new("legacy");
        sweep(&admin, &fx).await;
        setup(&admin, &fx).await;

        // ARM A - the shipped pre-fix statement, verbatim.
        admin
            .batch_execute(&format!(
                "GRANT {} TO {}",
                quote_ident(&fx.app_role),
                quote_ident(&fx.worker)
            ))
            .await
            .expect("legacy grant");
        assert_eq!(
            inherit_options(&admin, &fx).await,
            vec![true],
            "the legacy grant must record inherit_option = true"
        );
        let leaked = denied_as_worker(&admin, &fx, &bare_select(&fx))
            .await
            .expect(
                "the legacy grant MUST leak - if it does not, this fixture \
                 grants nothing and case 1's denial proves nothing",
            );
        assert_eq!(leaked, "tenant-secret");

        // ARM B - the role attribute, which is the intuitive fix and is inert.
        admin
            .batch_execute(&format!(
                "ALTER ROLE {} NOINHERIT",
                quote_ident(&fx.worker)
            ))
            .await
            .expect("alter the role attribute");
        let rolinherit: bool = admin
            .query_one_scalar(
                "SELECT rolinherit FROM pg_roles WHERE rolname = $1",
                &[&fx.worker],
            )
            .await
            .expect("read rolinherit");
        assert!(!rolinherit, "the ATTRIBUTE did flip");
        assert_eq!(
            inherit_options(&admin, &fx).await,
            vec![true],
            "but the MEMBERSHIP is untouched - this is the whole trap"
        );
        let still_leaked = denied_as_worker(&admin, &fx, &bare_select(&fx))
            .await
            .expect(
                "ALTER ROLE ... NOINHERIT must NOT close an existing grant; if \
                 this now denies, PostgreSQL changed its semantics and the \
                 WITH INHERIT FALSE rationale needs re-reading",
            );
        assert_eq!(still_leaked, "tenant-secret");

        teardown(&admin, &fx).await;
    }

    /// 4. CONVERGENCE, which is what makes this safe to ship without a
    /// migration. The `DO $runtime_dependents$` block re-runs on every apply,
    /// and existing databases carry the legacy inheriting membership.
    ///
    /// Measured rather than reasoned: re-granting with the option flips
    /// `inherit_option` IN PLACE, with no `REVOKE` and therefore no window in
    /// which the worker holds no membership at all. The reverse is a no-op, so
    /// a regression to the old statement cannot silently re-open a fenced
    /// database - asserted here because that asymmetry is the opposite of what
    /// "the last GRANT wins" would predict.
    #[compio::test]
    async fn re_granting_converges_a_legacy_database_and_does_not_regress() {
        let admin = admin_client().await;
        let fx = Fixture::new("converge");
        sweep(&admin, &fx).await;
        setup(&admin, &fx).await;

        // A database provisioned the old way.
        admin
            .batch_execute(&format!(
                "GRANT {} TO {}",
                quote_ident(&fx.app_role),
                quote_ident(&fx.worker)
            ))
            .await
            .expect("legacy grant");
        assert_eq!(inherit_options(&admin, &fx).await, vec![true]);

        // The next apply runs the production statement. No REVOKE anywhere.
        admin
            .batch_execute(&production_grant_for(&fx))
            .await
            .expect("re-run the production statement over the legacy membership");
        assert_eq!(
            inherit_options(&admin, &fx).await,
            vec![false],
            "re-granting must converge the existing membership in place"
        );
        denied_as_worker(&admin, &fx, &bare_select(&fx))
            .await
            .expect_err("a converged database must refuse the bare read");

        // And the reverse does NOT regress it.
        admin
            .batch_execute(&format!(
                "GRANT {} TO {}",
                quote_ident(&fx.app_role),
                quote_ident(&fx.worker)
            ))
            .await
            .expect("plain re-grant over a fenced membership");
        assert_eq!(
            inherit_options(&admin, &fx).await,
            vec![false],
            "a plain GRANT over a fenced membership is a no-op, not a re-open"
        );
        denied_as_worker(&admin, &fx, &bare_select(&fx))
            .await
            .expect_err("still refused after the plain re-grant");

        teardown(&admin, &fx).await;
    }

    /// 5. WHAT THE STATEMENT DOES NOT CONVERGE, stated as a test rather than a
    /// caveat in a comment.
    ///
    /// `pg_auth_members` is unique on `(roleid, member, GRANTOR)`. A second
    /// grantor adds a SECOND row and PostgreSQL takes the UNION, so one
    /// inheriting row re-opens the fence for the pair no matter what the other
    /// row says - and a plain `REVOKE`, even as superuser, removes only the
    /// issuing grantor's row.
    ///
    /// This is why `zeroship_worker`'s boot posture check counts EVERY
    /// inheriting membership row rather than asking whether "the" membership is
    /// fenced. A check shaped the latter way passes on exactly this state.
    #[compio::test]
    async fn a_second_grantor_re_opens_the_fence_and_a_plain_revoke_leaves_it_open() {
        let admin = admin_client().await;
        let fx = Fixture::new("grantor");
        sweep(&admin, &fx).await;
        setup(&admin, &fx).await;

        let second_grantor = format!("{}_g2", fx.worker);
        let g2_q = quote_ident(&second_grantor);
        let _ = admin
            .batch_execute(&format!("DROP ROLE IF EXISTS {g2_q}"))
            .await;
        admin
            .batch_execute(&format!(
                "CREATE ROLE {g2_q} LOGIN CREATEROLE;
                 GRANT {} TO {g2_q} WITH ADMIN OPTION;",
                quote_ident(&fx.app_role)
            ))
            .await
            .expect("create a second grantor with admin option");

        admin
            .batch_execute(&production_grant_for(&fx))
            .await
            .expect("the fenced grant, by the admin");
        assert_eq!(inherit_options(&admin, &fx).await, vec![false]);

        // The second grantor issues a PLAIN grant of the same pair.
        admin
            .batch_execute(&format!(
                "SET SESSION AUTHORIZATION {g2_q};
                 GRANT {} TO {};
                 RESET SESSION AUTHORIZATION;",
                quote_ident(&fx.app_role),
                quote_ident(&fx.worker)
            ))
            .await
            .expect("second grantor's plain grant");

        let options = inherit_options(&admin, &fx).await;
        assert_eq!(
            options.len(),
            2,
            "two grantors must produce two membership rows: {options:?}"
        );
        assert!(
            options.iter().any(|inherits| *inherits),
            "one of them inherits: {options:?}"
        );
        let leaked = denied_as_worker(&admin, &fx, &bare_select(&fx))
            .await
            .expect(
                "the UNION across grantors re-opens the fence - if this denies, \
                 the boot posture check could safely look at one row and its \
                 count-every-row shape is over-built",
            );
        assert_eq!(leaked, "tenant-secret");

        // A plain REVOKE as the admin removes only ITS row.
        admin
            .batch_execute(&format!(
                "REVOKE {} FROM {}",
                quote_ident(&fx.app_role),
                quote_ident(&fx.worker)
            ))
            .await
            .expect("admin revoke");
        assert_eq!(
            inherit_options(&admin, &fx).await,
            vec![true],
            "the other grantor's inheriting row survives a superuser REVOKE"
        );

        let _ = admin
            .batch_execute(&format!("DROP OWNED BY {g2_q} CASCADE"))
            .await;
        let _ = admin
            .batch_execute(&format!("DROP ROLE IF EXISTS {g2_q}"))
            .await;
        teardown(&admin, &fx).await;
    }
}
