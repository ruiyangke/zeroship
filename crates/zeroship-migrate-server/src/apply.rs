use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;
use zeroship_core::app_derivation;
use zeroship_core::app_id::AppId;
use zeroship_core::database_role::{per_app_role_name, PerAppRoleNameError};
use zeroship_schema::SchemaName;
use zeroship_migrate::apply::journal::DeployRecoveryScope;
use zeroship_migrate::{
    resolve_create_table_policy, Approval, ApprovalScope, DeclarativeApplyError, EngineError,
    ExecutorConfig, GuardConfig, IrAuthor, LiveSchema, LockMode, LoweredArtifact, MigrationBackend,
    MigrationEngine, MigrationIr, PlanStatusManifest, SealError, SealedPolicy, StatusError,
};
// PG-shaped surfaces live in the vendor crate now: the neutrality refactor moved
// them off the facade, so this PostgreSQL host names PostgreSQL rather than
// reaching for a re-export that deliberately no longer exists.
use crate::session::CompioPgSession;
use zeroship_migrate_policy::EffectivePolicy as PdpPolicy;
use zeroship_migrate_postgres::backend::drift_sql::snapshot_schema;
use zeroship_migrate_postgres::{PostgresBackend, DIALECT as POSTGRES};

/// The backends this host hands to every engine entry point.
///
/// `zeroship-migrate` is the composition root and the only crate that knows which
/// backends exist; a host takes the set rather than naming vendors itself, which
/// is what let the engine stop naming vendor crates at all. This service only
/// ever targets PostgreSQL, but that is pinned by the `POSTGRES` dialect passed
/// at each call site, NOT by narrowing the set: the engine resolves a backend by
/// `DialectId` out of whatever it was given, so a narrowed set would only change
/// which errors are reachable, not which backend runs.
const VENDORS: zeroship_migrate_backend::registry::VendorSet = zeroship_migrate::shipping_vendors();

use crate::policy::{
    confined_guard_policy_for_schema, CreatorPolicyDraft, EffectivePolicy, ManagedPolicyConfig,
    ManagedPolicyError, SealVerifier,
};
use crate::provisioning::{
    exec_retry, migrator_executor_config, provision_audit_unmask_table, provision_migrator,
    ProvisionRoleError, AUDIT_UNMASK_TABLE, RESERVED_SYSTEM_TABLE_PREFIX,
};
use crate::publication::{reconcile_app_publication, PublicationError};
use crate::schema_apply_store::{
    SchemaApplyInput, SchemaApplyStore, SchemaApplyStoreError, TerminalTransition,
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
    #[error("inspect migration database schema: {0}")]
    InspectSchema(compio_postgres::Error),
    /// `reason` is a rendered string rather than a `#[source]` because
    /// `zeroship_schema::query::QueryError` implements `Display` but not
    /// `std::error::Error`, so it cannot be a source in this chain.
    #[error("app schema name {schema:?} is not a legal identifier: {reason}")]
    SchemaName { schema: String, reason: String },
    /// Rendered through `as_str` rather than `Display`: [`AppId`] deliberately
    /// implements no `Display`, so every place an id becomes text is greppable.
    #[error("database {} has not been created", database_id.as_str())]
    DatabaseNotCreated { database_id: AppId },
    #[error("migration role provision: {0}")]
    ProvisionRole(#[from] ProvisionRoleError),
    #[error("runtime app role provision: {0}")]
    ProvisionRuntimeRole(ProvisionRuntimeRoleError),
    #[error("unmask audit table provision: {0}")]
    ProvisionAuditUnmask(compio_postgres::Error),
    #[error("app publication provision: {0}")]
    ProvisionPublication(#[from] PublicationError),
    #[error("{action} migration project advisory lock: {source}")]
    ProjectLock {
        action: &'static str,
        #[source]
        source: zeroship_migrate::ApplyError,
    },
    #[error("migration history attestation: {0}")]
    HistoryAttestation(#[from] StatusError),
    #[error(
        "submitted migration set is incomplete: missing journaled versions: {missing}",
        missing = .missing_versions.join(", ")
    )]
    IncompleteHistory { missing_versions: Vec<String> },
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
/// The row in `zeroship.app_schema_applies` opens only after guarded preparation
/// and complete-history attestation, but before creator DDL. A surviving request
/// with a reachable control store then closes it through exactly one terminal
/// transition: `mark_applied` on success or `mark_failed` on error. A process
/// death or terminal store failure can leave it `submitted`: the ledger and app
/// schema may live on different DSNs, so no transaction spans them. No serving
/// path reads an open row; the deploy gate reads only `applied` rows.
pub async fn apply_ir_documents(
    provision_dsn: &str,
    tmp_root: &Path,
    app_id: &AppId,
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

    // THE SERVICE'S ONE APP-ID-TO-SCHEMA DERIVATION. Everything downstream that
    // means "the physical schema" takes the [`SchemaName`], and everything that
    // means "the tenant" keeps taking `app_id`. `schema_name` is the identity on
    // the printed id today, so `SchemaName::new` cannot refuse it; the refusal is
    // handled rather than unwrapped because the day the schema stops being the
    // app id, this line is where the new derivation - and its failure - lands.
    let schema_text = app_derivation::schema_name(app_id);
    let schema =
        SchemaName::new(&schema_text).map_err(|reason| ApplyRequestError::SchemaName {
            schema: schema_text.clone(),
            reason: reason.to_string(),
        })?;

    // (a) DRIVER: open a native compio session, wrap it in the adapter's
    // `CompioPgSession`, and drive the published engine over it. Provisioning
    // runs over the SAME raw compio `Client`, borrowed back via `session.client()`.
    let session = CompioPgSession::connect(provision_dsn)
        .await
        .map_err(ApplyRequestError::Connect)?;
    let schema_exists: bool = session
        .client()
        .query_one(
            "SELECT EXISTS (\
                 SELECT 1 FROM pg_catalog.pg_namespace WHERE nspname = $1\
             )",
            &[&schema_text],
        )
        .await
        .map_err(ApplyRequestError::InspectSchema)?
        .get(0);
    if !schema_exists {
        return Err(ApplyRequestError::DatabaseNotCreated {
            database_id: app_id.clone(),
        });
    }

    // The existence fence is above all filesystem, role, audit, lock, and
    // ledger side effects. A refused apply therefore leaves no database state
    // for a retry or deploy gate to mistake for a completed lifecycle step.
    let dir = write_ir_documents(tmp_root, request)?;
    let request_body = serde_json::to_value(request)
        .map_err(|err| ApplyRequestError::EncodeRequest(err.to_string()))?;
    // The executor re-vets rendered SQL at apply time from its own policy, so it must
    // carry the same no-inject confined guard charter as guarded lower. The composed
    // inject-bearing policy remains separate and is passed explicitly to shape
    // resolution and `IrAuthor` below.
    // SCHEMA, still spelled `&str`. `migrator_executor_config`,
    // `provision_audit_unmask_table` and `prepare_ir_documents` all mean the
    // physical schema, and typing them reaches the engine`s `ExecutorConfig`,
    // which takes owned `String`s. The downgrade is explicit and greppable
    // (`schema.as_str()`) rather than a `&str` that either identity satisfies.
    let (exec_cfg, role) = migrator_executor_config(schema.as_str())?;
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
    provision_migrator(session.client(), &exec_cfg).await?;
    // The per-app unmask audit table, which the worker WRITES and creates no
    // longer. Until this change `crud/unmask.rs` emitted its `CREATE TABLE` and
    // three `CREATE INDEX` on every `unmask()` call; that was the last live DDL
    // in the data plane.
    //
    // BEFORE `provision_runtime_app_role` below, and that is not cosmetic. That
    // function explicitly looks up this table and its `BIGSERIAL` sequence,
    // clears every additive privilege, then grants only table INSERT and
    // sequence USAGE. Created after the last call, both lookups would be no-ops
    // and the table would be unreachable to the only process that writes it.
    provision_audit_unmask_table(session.client(), schema.as_str())
        .await
        .map_err(ApplyRequestError::ProvisionAuditUnmask)?;
    let backend = PostgresBackend::new_generic(&session);
    backend
        .acquire_project_lock(&exec_cfg)
        .await
        .map_err(|source| ApplyRequestError::ProjectLock {
            action: "acquire",
            source,
        })?;

    // The one project-lock bracket binds all four facts the deploy ledger relies
    // on: the catalog snapshot used to lower, the complete supplied manifest set,
    // the journal coverage verdict, and the terminal ledger timestamp. Releasing
    // before `mark_applied` would let an older concurrent request stamp a newer
    // `applied_at` after a later schema had already completed.
    let result = async {
        // PREPARATION runs before any ledger row or creator DDL. It lowers every
        // document under the guard, refuses a denied plan, and retains those exact
        // artifacts for apply so attestation and execution cannot disagree.
        let prepared =
            prepare_ir_documents(&session, &exec_cfg, schema.as_str(), dir.path(), &policy)
                .await?;
        attest_complete_history(&backend, &exec_cfg, &prepared).await?;

        // Coverage refusal happens above this line. A truncated request therefore
        // leaves no submitted, failed, or applied row that could become the deploy
        // guard's newest ledger fact.
        schema_apply_store
            .record_submitted(SchemaApplyInput {
                app_id,
                migration_id,
                principal_id,
                request_body,
                effective_profile: &policy.managed,
                ceiling_id: &policy.ceiling_id,
                ceiling_version: policy.ceiling_version,
                descriptor_sha256: &request.descriptor_sha256,
            })
            .await?;

        let apply_result = run_apply(
            &session,
            &backend,
            policy_config,
            &policy,
            app_id,
            &schema,
            &prepared,
            &exec_cfg,
            &role,
            principal_id,
        )
        .await;

        match apply_result {
            Ok(outcome) => {
                // WRITTEN EVEN WHEN `outcome.applied` IS EMPTY. A re-run that applied
                // nothing is what an app whose descriptor bytes moved without a
                // schema change (an engine upgrade, a codegen fix) uses to become
                // deployable again. Coverage, not emptiness, decides whether the row
                // is safe to write.
                match schema_apply_store
                    .mark_applied(app_id, migration_id, &outcome.applied)
                    .await
                {
                    Ok(transition) => {
                        if transition == TerminalTransition::Lost {
                            // The DDL is committed and cannot be taken back, but a
                            // concurrent path failed this migration while the engine
                            // was applying it, so the row reads `failed`. The record
                            // now contradicts the database it describes and only an
                            // operator can reconcile them.
                            tracing::error!(
                                app_id = app_id.as_str(),
                                migration_id = %migration_id,
                                "migrate-server: apply lost the terminal transition to a \
                                 concurrent failure - schema changes are committed but the row \
                                 reads failed"
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
                mark_apply_failed(schema_apply_store, app_id, migration_id, &err.to_string())
                    .await;
                Err(err)
            }
        }
    }
    .await;

    let release = backend.release_project_lock(&exec_cfg).await;
    match (result, release) {
        (Ok(response), Ok(())) => Ok(response),
        (Ok(_), Err(source)) => Err(ApplyRequestError::ProjectLock {
            action: "release",
            source,
        }),
        (Err(error), Ok(())) => Err(error),
        (Err(error), Err(release_error)) => {
            tracing::warn!(
                error = %release_error,
                project = %exec_cfg.project_id,
                "migrate-server: failed to release project lock after request failure"
            );
            Err(error)
        }
    }
}

/// Run every fallible schema/apply operation after the ledger row opens and before
/// its terminal transition, while the caller holds the project lock.
///
/// The caller awaits this future without `?`, then routes its one result through
/// the terminal ledger transition. Adding another post-submit operation here
/// cannot create a new unclosed return path in [`apply_ir_documents`].
#[allow(clippy::result_large_err, clippy::too_many_arguments)]
async fn run_apply(
    session: &CompioPgSession,
    backend: &PostgresBackend<'_, CompioPgSession>,
    policy_config: &ManagedPolicyConfig,
    apply_policy: &EffectivePolicy,
    app_id: &AppId,
    schema: &SchemaName,
    prepared: &[PreparedIrDocument],
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
        app_id = app_id.as_str(),
        schema = %schema.as_str(),
        ceiling_id = %sealed_policy.ceiling_id,
        ceiling_version = sealed_policy.ceiling_version,
        "migrate-server: applying IR under sealed managed migration policy"
    );
    let applied_by = format!("migrate-server:{principal_id}");
    provision_runtime_app_role(session.client(), app_id, schema, role)
        .await
        .map_err(ApplyRequestError::ProvisionRuntimeRole)?;
    let applied = apply_sealed(
        backend,
        sealed_policy.sealed,
        &sealed_policy.verifier,
        prepared,
        exec_cfg,
        &apply_policy.policy,
        Approval::None,
        &applied_by,
    )
    .await;
    // RE-PROVISION ON BOTH PATHS, AND `?` THE APPLY ONLY AFTERWARDS. A failed
    // apply still commits DDL - the engine bootstraps its journal before the
    // first migration runs, and `run_backfill` creates
    // `__zeroship_schema_backfills` on the way in - and the call above has
    // already handed the runtime role DML on every table then present. Ending
    // here on `?` used to leave those reserved relations writable by creator
    // code until some LATER apply happened to succeed. The sweep inside this
    // call is what strips them, so it has to run whatever the apply returned.
    let reprovisioned = provision_runtime_app_role(session.client(), app_id, schema, role)
        .await
        .map_err(ApplyRequestError::ProvisionRuntimeRole);
    let outcome = applied?;
    reprovisioned?;
    // NO LONGER AMBIGUOUS. This used to hand `reconcile_app_publication` one
    // `&str` that it spent on BOTH identities - the publication name (tenant-keyed)
    // and the `pg_namespace.nspname` filter (schema-keyed) - because the app id
    // and the schema were the same bytes. It now takes the tenant and asks
    // `app_derivation` for each derived name in turn.
    reconcile_app_publication(session.client(), app_id).await?;
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

/// Apply prepared `.ir.json` documents through a sealed shared-infra policy.
///
/// Verifies the in-process MAC + binding (tamper/staleness fail-closed), then
/// applies the exact guarded-lower artifacts whose manifests passed journal
/// coverage attestation. The caller owns the project lock for the whole call.
#[allow(clippy::too_many_arguments)]
async fn apply_sealed(
    backend: &PostgresBackend<'_, CompioPgSession>,
    sealed: SealedPolicy,
    verifier: &SealVerifier,
    prepared: &[PreparedIrDocument],
    exec_cfg: &ExecutorConfig,
    policy: &PdpPolicy,
    approval: Approval,
    applied_by: &str,
) -> Result<SealedApplyOutcome, SealedApplyError> {
    verifier.verify(&sealed, policy)?;
    apply_prepared_ir_documents(backend, prepared, exec_cfg, approval, applied_by)
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

/// One guarded-lower artifact retained from the locked preparation pass.
struct PreparedIrDocument {
    file: String,
    artifact: LoweredArtifact,
}

/// Apply the exact prepared documents whose complete manifest set was attested.
///
/// The caller owns the project lock. Every engine call therefore uses
/// [`LockMode::AlreadyHeld`], including the first document.
async fn apply_prepared_ir_documents(
    backend: &PostgresBackend<'_, CompioPgSession>,
    prepared: &[PreparedIrDocument],
    exec_cfg: &ExecutorConfig,
    approval: Approval,
    applied_by: &str,
) -> Result<SealedApplyOutcome, IrApplyError> {
    let mut aggregate = SealedApplyOutcome::default();
    for document in prepared {
        let artifact = &document.artifact;
        let recovery_scope: Option<&DeployRecoveryScope<'_>> = None;
        let outcome = MigrationEngine::new(VENDORS)
            .apply_plan_with_touched_and_depends_scoped(
                &artifact.plan.steps,
                &artifact.touched_tables,
                &artifact.depends_on,
                approval,
                &ApprovalScope::All,
                backend,
                exec_cfg,
                applied_by,
                LockMode::AlreadyHeld,
                recovery_scope,
            )
            .await
            .map_err(|source| IrApplyError::Apply {
                file: document.file.clone(),
                source,
            })?;
        aggregate.applied.extend(outcome.applied.applied);
        aggregate.skipped.extend(outcome.applied.skipped);
        aggregate.pending_contract.extend(
            outcome
                .pending_contract
                .iter()
                .map(|migration| migration.version.as_str().to_string()),
        );
    }
    Ok(aggregate)
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
    app_id: &AppId,
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

/// Lower every document under the guard, refuse a denied plan, and retain the
/// exact artifacts for journal attestation and apply.
///
/// It used to also classify which versions needed operator approval; that
/// classification had exactly one consumer, an approval state machine no caller
/// could drive, and both are gone. A bundle whose LAST document is denied must be
/// refused before the DDL of the earlier documents commits, so preparation walks
/// the whole set first.
///
/// This function runs while the caller holds the project lock. Keeping the owned
/// artifacts means status and apply consume one lowering result, not two catalog
/// snapshots that merely ought to agree.
async fn prepare_ir_documents(
    session: &CompioPgSession,
    exec_cfg: &ExecutorConfig,
    schema: &str,
    migrations_dir: &Path,
    policy: &EffectivePolicy,
) -> Result<Vec<PreparedIrDocument>, ApplyRequestError> {
    let files = discover_ir_files(migrations_dir)?;
    let guard_cfg = guard_config_for_managed(schema);
    let mut state = postgres_ir_apply_state(session, exec_cfg, schema)
        .await
        .map_err(IrApplyError::Snapshot)?;
    let engine = MigrationEngine::new(VENDORS);
    let mut prepared = Vec::with_capacity(files.len());

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
        // Fold the effective table-shape profile before guarded lower so the
        // retained artifact has the managed system shape the executor will apply.
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
        for table in &lowered.created_tables {
            state
                .registry
                .entry(table.clone())
                .or_insert_with(|| schema.to_string());
            state.live_schema.tables.insert(table.clone());
        }
        prepared.push(PreparedIrDocument {
            file,
            artifact: lowered,
        });
    }

    Ok(prepared)
}

/// Require the supplied complete manifest set to cover every unaccounted-for
/// net journal identity.
///
/// The ordinary executor deliberately cannot make this decision because its
/// per-plan input is not the host's complete set. Here the server has every
/// prepared document and holds the same project lock the subsequent apply uses.
async fn attest_complete_history(
    backend: &PostgresBackend<'_, CompioPgSession>,
    exec_cfg: &ExecutorConfig,
    prepared: &[PreparedIrDocument],
) -> Result<(), ApplyRequestError> {
    // Bootstrap after preparation so a malformed creator bundle still fails before
    // journal DDL, but before status so an interrupted partial bootstrap is repaired
    // rather than mistaken for an empty history. The outer advisory lock serializes
    // the first bootstrap.
    backend
        .ensure_journal(exec_cfg)
        .await
        .map_err(StatusError::Journal)?;
    let mut manifests = Vec::with_capacity(prepared.len());
    for document in prepared {
        manifests.push(PlanStatusManifest::from_applied_plan(
            &document.artifact.plan,
            &document.artifact.depends_on,
        )?);
    }
    let status = zeroship_migrate::ops::status::status_plans_via_backend_locked(
        backend, exec_cfg, &manifests,
    )
    .await?;
    let mut missing_versions: Vec<String> = status
        .unexpected_journal
        .into_iter()
        .map(|entry| entry.version)
        .collect();
    missing_versions.sort();
    missing_versions.dedup();
    if missing_versions.is_empty() {
        Ok(())
    } else {
        Err(ApplyRequestError::IncompleteHistory { missing_versions })
    }
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
        let bytes =
            serde_json::to_vec_pretty(&doc.body).map_err(|source| ApplyRequestError::Write {
                path: path.clone(),
                source: std::io::Error::other(source.to_string()),
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
    app_id: &AppId,
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
                app_id = app_id.as_str(),
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
        ApplyRequestError::IncompleteHistory { .. } => (
            ntex::http::StatusCode::CONFLICT,
            "migration_history_incomplete",
        ),
        ApplyRequestError::DatabaseNotCreated { .. } => {
            (ntex::http::StatusCode::CONFLICT, "database_not_created")
        }
        // Not a creator fault and not retryable: the caller supplied an app id,
        // the service derived a schema name from it, and the derivation produced
        // something PostgreSQL cannot name. That is a platform defect.
        ApplyRequestError::SchemaName { .. } => (
            ntex::http::StatusCode::INTERNAL_SERVER_ERROR,
            "app_schema_name_invalid",
        ),
        ApplyRequestError::HistoryAttestation(
            StatusError::Ordering(_) | StatusError::PlanManifest(_),
        ) => (
            ntex::http::StatusCode::UNPROCESSABLE_ENTITY,
            "migration_invalid",
        ),
        ApplyRequestError::Preflight(source) => ir_apply_error_kind(source),
        ApplyRequestError::Apply(source) => sealed_apply_error_kind(source),
        ApplyRequestError::TempDir(_)
        | ApplyRequestError::Write { .. }
        | ApplyRequestError::Policy(_)
        | ApplyRequestError::SchemaApplyStore(_)
        | ApplyRequestError::EncodeRequest(_)
        | ApplyRequestError::Connect(_)
        | ApplyRequestError::InspectSchema(_)
        | ApplyRequestError::ProvisionRole(_)
        | ApplyRequestError::ProvisionRuntimeRole(_)
        | ApplyRequestError::ProvisionAuditUnmask(_)
        | ApplyRequestError::ProvisionPublication(_)
        | ApplyRequestError::ProjectLock { .. }
        | ApplyRequestError::HistoryAttestation(_) => (
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
///
/// # The journal schema is named by the seam, not composed here
///
/// This used to compose `format!("app_{}", schema.as_str())` - a SECOND spelling
/// of a name whose first spelling is
/// [`crate::provisioning::workflow_journal_schema_name`], and one derived from
/// the SCHEMA rather than from the tenant. The two agreed only because the
/// schema was the bare uuid and the journal schema was that uuid with an `app_`
/// prefix. With the app id itself printed as `app_<base62>` the composition
/// would have produced `app_app_<base62>` while the workflow plugin read
/// somewhere else, and nothing would have failed: the deploy would write its
/// journal where nothing looks for it. It takes the tenant and asks the one
/// derivation.
fn runtime_dependents_sql_for_role(
    app_id: &AppId,
    runtime_role: &str,
) -> String {
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
        journal = crate::provisioning::workflow_journal_schema_sql(
            &crate::provisioning::workflow_journal_schema_name(app_id)
        ),
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
    revoke_reserved: String,
    revoke_audit: String,
    grant_audit: String,
    dependents: String,
}

impl RuntimeRoleProvisioningSql {
    /// The exact, unquoted role identifier carried by every statement.
    #[must_use]
    pub fn role_name(&self) -> &str {
        &self.role_name
    }

    /// The plan, in execution order. THE ORDER IS THE PRIVILEGE.
    ///
    /// `grants` deliberately hands the runtime role DML on every table in the
    /// app schema, which includes the platform's own reserved `__zeroship_*`
    /// relations. `revoke_reserved` takes all of that back - the migration
    /// journal included - and `revoke_audit` + `grant_audit` then re-issue the
    /// single narrow exception the worker needs. Move any of the last four
    /// above `grants` and the wide grant simply overwrites them.
    fn statements(&self) -> [&str; 6] {
        [
            &self.create_role,
            &self.grants,
            &self.revoke_reserved,
            &self.revoke_audit,
            &self.grant_audit,
            &self.dependents,
        ]
    }
}

/// Strip the runtime role's reach on every reserved `__zeroship_*` relation in
/// the app schema.
///
/// # Why this exists
///
/// The migration journal lives IN the creator's schema for locality -
/// `"<app_uuid>".__zeroship_schema_migrations` and its siblings. `grants` above
/// says `ON ALL TABLES IN SCHEMA`, and `ALL` includes those. That handed the
/// role creator code executes under INSERT/UPDATE/DELETE on the migration
/// service's own record of its work: an app could forge an `applied` event, or
/// delete the two-phase `..._inflight` marker the executor's recovery path
/// reads on the next apply.
///
/// This is NOT the masking argument from the per-column grant discussion. A
/// creator reading their own masked data is their data under their own policy.
/// The ledger is not creator data - it is state a SEPARATE SERVICE writes and
/// the tenant must not be able to forge, which is exactly the one shape the
/// `__zeroship_` prefix was introduced to fence.
///
/// # Why a full revoke, SELECT included
///
/// The data plane never reads the journal. `zeroship-data-engine`'s
/// `descriptor.rs` is its sole schema authority (the runtime descriptor rides in
/// the `.zship`), and no CRUD, transaction or CDC path in `zeroship-data-engine`
/// or `zeroship-plugin-db` names any journal table. Nothing is left to grant.
///
/// # Why the sweep is by PREFIX and takes the audit table too
///
/// A named list would go stale the day the engine adds a seventh journal table;
/// the prefix is the contract creator-declared collections are refused from, so
/// matching it catches whatever the engine creates next. The unmask audit table
/// shares the prefix and is swept with the rest - it is re-granted immediately
/// afterwards by the dedicated `revoke_audit` / `grant_audit` recipe, which is
/// why that pair must stay AFTER this statement. A second reserved table the
/// worker may write therefore has to be given its own explicit recipe, rather
/// than inheriting reach from a wildcard.
fn revoke_runtime_reserved_privileges_sql(schema: &SchemaName, runtime_role: &str) -> String {
    let schema_lit = quote_lit(schema.as_str());
    let role_lit = quote_lit(runtime_role);
    let prefix_lit = quote_lit(RESERVED_SYSTEM_TABLE_PREFIX);
    format!(
        "DO $runtime_reserved_revoke$ \
         DECLARE \
           reserved_rel record; \
         BEGIN \
           FOR reserved_rel IN \
             SELECT n.nspname, c.relname, c.relkind \
               FROM pg_class c \
               JOIN pg_namespace n ON n.oid = c.relnamespace \
              WHERE n.nspname = '{schema_lit}' \
                AND c.relkind IN ('r', 'p', 'v', 'm', 'f', 'S') \
                AND left(c.relname, {prefix_len}) = '{prefix_lit}' \
           LOOP \
             IF reserved_rel.relkind = 'S' THEN \
               EXECUTE format( \
                 'REVOKE ALL PRIVILEGES ON SEQUENCE %I.%I FROM %I', \
                 reserved_rel.nspname, reserved_rel.relname, '{role_lit}' \
               ); \
             ELSE \
               EXECUTE format( \
                 'REVOKE ALL PRIVILEGES ON TABLE %I.%I FROM %I', \
                 reserved_rel.nspname, reserved_rel.relname, '{role_lit}' \
               ); \
             END IF; \
           END LOOP; \
         END \
         $runtime_reserved_revoke$",
        prefix_len = RESERVED_SYSTEM_TABLE_PREFIX.len(),
    )
}

/// Clear every additive privilege from the runtime role's audit objects.
///
/// This is deliberately a separate statement from the narrow grant below. If
/// the audit table is malformed and the grant fails, the deny remains committed
/// instead of rolling back with it. The lookup is a no-op before the audit table
/// exists, preserving the role provisioner's idempotent schema-only shape.
fn revoke_runtime_audit_privileges_sql(schema: &SchemaName, runtime_role: &str) -> String {
    let schema_lit = quote_lit(schema.as_str());
    let role_lit = quote_lit(runtime_role);
    let audit_lit = quote_lit(AUDIT_UNMASK_TABLE);
    format!(
        "DO $runtime_audit_revoke$ \
         DECLARE \
           audit_rel record; \
           sequence_rel record; \
         BEGIN \
           SELECT n.nspname, c.relname, c.relkind \
             INTO audit_rel \
             FROM pg_class c \
             JOIN pg_namespace n ON n.oid = c.relnamespace \
            WHERE n.nspname = '{schema_lit}' \
              AND c.relname = '{audit_lit}' \
              AND c.relkind IN ('r', 'p', 'v', 'm', 'f', 'S'); \
           IF FOUND THEN \
             IF audit_rel.relkind = 'S' THEN \
               EXECUTE format( \
                 'REVOKE ALL PRIVILEGES ON SEQUENCE %I.%I FROM %I', \
                 audit_rel.nspname, audit_rel.relname, '{role_lit}' \
               ); \
             ELSE \
               EXECUTE format( \
                 'REVOKE ALL PRIVILEGES ON TABLE %I.%I FROM %I', \
                 audit_rel.nspname, audit_rel.relname, '{role_lit}' \
               ); \
               IF audit_rel.relkind IN ('r', 'p') THEN \
                 SELECT n.nspname, c.relname \
                   INTO sequence_rel \
                   FROM pg_class c \
                   JOIN pg_namespace n ON n.oid = c.relnamespace \
                  WHERE c.oid = pg_get_serial_sequence( \
                          format('%I.%I', audit_rel.nspname, audit_rel.relname), \
                          'id' \
                        )::regclass \
                    AND c.relkind = 'S'; \
                 IF FOUND THEN \
                   EXECUTE format( \
                     'REVOKE ALL PRIVILEGES ON SEQUENCE %I.%I FROM %I', \
                     sequence_rel.nspname, sequence_rel.relname, '{role_lit}' \
                   ); \
                 END IF; \
               END IF; \
             END IF; \
           END IF; \
         END \
         $runtime_audit_revoke$"
    )
}

/// Give the runtime role only the privileges needed to append an audit row.
///
/// PostgreSQL grants are additive, so this must execute after
/// [`revoke_runtime_audit_privileges_sql`]. A real audit table without the
/// `BIGSERIAL` sequence required by the data-plane INSERT is rejected.
fn grant_runtime_audit_append_privileges_sql(schema: &SchemaName, runtime_role: &str) -> String {
    let schema_lit = quote_lit(schema.as_str());
    let role_lit = quote_lit(runtime_role);
    let audit_lit = quote_lit(AUDIT_UNMASK_TABLE);
    format!(
        "DO $runtime_audit_grant$ \
         DECLARE \
           audit_rel record; \
           sequence_rel record; \
         BEGIN \
           SELECT n.nspname, c.relname \
             INTO audit_rel \
             FROM pg_class c \
             JOIN pg_namespace n ON n.oid = c.relnamespace \
            WHERE n.nspname = '{schema_lit}' \
              AND c.relname = '{audit_lit}' \
              AND c.relkind IN ('r', 'p'); \
           IF FOUND THEN \
             SELECT n.nspname, c.relname \
               INTO sequence_rel \
               FROM pg_class c \
               JOIN pg_namespace n ON n.oid = c.relnamespace \
              WHERE c.oid = pg_get_serial_sequence( \
                      format('%I.%I', audit_rel.nspname, audit_rel.relname), \
                      'id' \
                    )::regclass \
                AND c.relkind = 'S'; \
             IF NOT FOUND THEN \
               RAISE EXCEPTION 'serial sequence missing for %.%.id', \
                 audit_rel.nspname, audit_rel.relname; \
             END IF; \
             EXECUTE format( \
               'GRANT USAGE ON SEQUENCE %I.%I TO %I', \
               sequence_rel.nspname, sequence_rel.relname, '{role_lit}' \
             ); \
             EXECUTE format( \
               'GRANT INSERT ON TABLE %I.%I TO %I', \
               audit_rel.nspname, audit_rel.relname, '{role_lit}' \
             ); \
           END IF; \
         END \
         $runtime_audit_grant$"
    )
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
/// # Why the parameter is a [`SchemaName`] and not a `&str`
///
/// The role this composes is what the data plane's `SET LOCAL ROLE` must name,
/// and the data plane composes that name on its own side
/// (`zeroship_data_postgres::pg_session_sql::tx_session_setup_sql`). While both
/// took `&str`, "they agree" was only ever true of the value each caller
/// happened to hold: the parameter here is the SCHEMA and the one there was the
/// TENANT, and the two are the same string only until the physical schema stops
/// being the app id. Both now take the same type, so a caller reaching for the
/// wrong identity is a compile error rather than a `SET LOCAL ROLE` naming a
/// role nobody created.
///
/// # Preconditions
///
/// `migrator_role` must contain no single quote. `schema` cannot: [`SchemaName`]
/// admits only `[A-Za-z0-9_-]`.
///
/// The `DO` block below embeds their `quote_ident` forms inside single-quoted
/// `EXECUTE '...'` strings. `quote_ident` doubles internal double-quotes and does
/// nothing to single ones, so a name containing `'` would terminate the EXECUTE
/// literal early and the remainder would be parsed as SQL.
///
/// That is safe today for `migrator_role` only because the production caller
/// derives it from the schema, and derives `role_name` through
/// [`per_app_role_name`]. Neither can carry a quote. That guarantee lives far
/// from this function and nothing here enforces it, so a future caller must
/// preserve the precondition.
///
/// The durable fix is to stop pre-interpolating and let the block quote its own
/// identifiers with `format('%I', ...)`.
pub fn runtime_role_provisioning_sql(
    app_id: &AppId,
    schema: &SchemaName,
    migrator_role: &str,
) -> Result<RuntimeRoleProvisioningSql, PerAppRoleNameError> {
    let schema_q = schema.quoted();
    let role_name = per_app_role_name(schema.as_str())?;
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
        //
        // INTERIM, NOT THE INTENDED END STATE. This deliberately re-widens the
        // table authority that 6035c8601 narrowed. It exists because the
        // replacement apply-time per-column grant producer was never built;
        // without either producer, a successfully migrated app cannot use any
        // creator table. This grants SELECT, INSERT, UPDATE, and DELETE on every
        // existing table in the app schema and the same defaults on tables the
        // migrator creates later. It does not exclude masked or encrypted raw
        // columns. Replace these two table statements with the real per-column
        // producer.
        //
        // IT NO LONGER LEAVES THE MIGRATION JOURNAL EXPOSED. `ALL TABLES` still
        // sweeps the reserved `__zeroship_*` relations in - PostgreSQL has no
        // "all except" form - so the wide grant is UNDONE for exactly that
        // namespace by `revoke_runtime_reserved_privileges_sql`, which runs next,
        // before the audit recipe re-grants the one exception.
        //
        // THE `ALTER DEFAULT PRIVILEGES` LINES NEED NO MATCHING EXCLUSION, and
        // that is measured rather than assumed: an entry `FOR ROLE <migrator>`
        // fires only for objects the MIGRATOR creates, and the engine bootstraps
        // its journal on the migration service's own admin session with no `SET
        // ROLE` (`zeroship_migrate_postgres::backend::journal_sql::ensure_journal`,
        // `backfill_sql::ensure_progress`). Measured on PostgreSQL 18.6: with
        // these entries installed, a table created by the migrator came out with
        // the runtime role holding INSERT and one created by the admin came out
        // without it. Should that ever change, the sweep still runs on both of the
        // apply path's `provision_runtime_app_role` calls - including the one on
        // the failure path - so a journal table born mid-apply is stripped before
        // the apply returns either way.
        "GRANT USAGE ON SCHEMA {schema_q} TO {role_q};
         GRANT SELECT, INSERT, UPDATE, DELETE ON ALL TABLES IN SCHEMA {schema_q} TO {role_q};
         GRANT USAGE, SELECT ON ALL SEQUENCES IN SCHEMA {schema_q} TO {role_q};
         ALTER DEFAULT PRIVILEGES FOR ROLE {migrator_q} IN SCHEMA {schema_q}
             GRANT SELECT, INSERT, UPDATE, DELETE ON TABLES TO {role_q};
         ALTER DEFAULT PRIVILEGES FOR ROLE {migrator_q} IN SCHEMA {schema_q}
             GRANT USAGE, SELECT ON SEQUENCES TO {role_q};"
    );
    let revoke_reserved = revoke_runtime_reserved_privileges_sql(schema, &role_name);
    let revoke_audit = revoke_runtime_audit_privileges_sql(schema, &role_name);
    let grant_audit = grant_runtime_audit_append_privileges_sql(schema, &role_name);

    // The migration identity creates no platform role here. It only delegates
    // to the narrow roles that the platform role migration precreated.
    let dependents = runtime_dependents_sql_for_role(app_id, &role_name);
    Ok(RuntimeRoleProvisioningSql {
        role_name,
        create_role,
        grants,
        revoke_reserved,
        revoke_audit,
        grant_audit,
        dependents,
    })
}

async fn provision_runtime_app_role(
    conn: &compio_postgres::Client,
    app_id: &AppId,
    schema: &SchemaName,
    migrator_role: &str,
) -> Result<(), ProvisionRuntimeRoleError> {
    let provisioning = runtime_role_provisioning_sql(app_id, schema, migrator_role)?;
    for statement in provisioning.statements() {
        exec_retry(conn, statement).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    /// The fixed app id every SQL-shape assertion in this module derives from.
    ///
    /// A frozen literal rather than a mint, because those assertions quote the
    /// derived role and schema names VERBATIM. A fresh id per run would force
    /// each expectation to become a `format!` over the same derivation the code
    /// under test uses, which asserts that the code agrees with itself.
    ///
    /// It is the base62 rendering of `0191e7a2-b3c4-4d5e-8f90-123456789abc`, the
    /// uuid these assertions carried before the id became text, so the fixture
    /// names the same tenant it always did.
    const FIXTURE_APP_ID: &str = "app_02xfbOcLMnlN2aR6iBlNi0";

    fn fixture_app_id() -> AppId {
        AppId::parse(FIXTURE_APP_ID).expect("the fixture app id must be canonical")
    }

    /// The fixture app's physical schema, through the one derivation.
    fn fixture_schema() -> SchemaName {
        SchemaName::new(&app_derivation::schema_name(&fixture_app_id()))
            .expect("an app id is a legal schema identifier")
    }

    /// The reserved sweep runs AFTER the wide grant and BEFORE the audit recipe.
    ///
    /// Both edges are the whole mechanism, and both are silent when broken:
    /// `PostgreSQL` grants are additive and last-writer-wins, so a sweep hoisted
    /// above `grants` leaves the journal writable and a sweep dropped below
    /// `grant_audit` leaves the audit table unwritable. Positions, not mere
    /// presence.
    ///
    /// This says nothing about whether the server honoured the REVOKE -
    /// `live_reserved_journal_privileges` is the only thing that can.
    #[test]
    fn the_reserved_sweep_sits_between_the_wide_grant_and_the_audit_recipe() {
        let provisioning = runtime_role_provisioning_sql(
            &fixture_app_id(),
            &fixture_schema(),
            "zs_migrator_fixture",
        )
        .expect("test runtime role name");
        let statements = provisioning.statements();
        let position = |needle: &str| {
            statements
                .iter()
                .position(|s| s.contains(needle))
                .unwrap_or_else(|| panic!("no statement contains {needle:?}: {statements:?}"))
        };
        let wide_grant = position("ON ALL TABLES IN SCHEMA");
        let sweep = position("$runtime_reserved_revoke$");
        let audit_revoke = position("$runtime_audit_revoke$");
        let audit_grant = position("$runtime_audit_grant$");
        assert!(
            wide_grant < sweep,
            "the sweep must undo the wide grant, not be undone by it: \
             grant at {wide_grant}, sweep at {sweep}"
        );
        assert!(
            sweep < audit_revoke && audit_revoke < audit_grant,
            "the audit recipe re-grants the one reserved table the worker writes, \
             so it must follow the sweep: sweep at {sweep}, revoke at \
             {audit_revoke}, grant at {audit_grant}"
        );
    }

    /// The sweep matches the reserved namespace by PREFIX, with the length the
    /// prefix actually has.
    ///
    /// `left(relname, N)` with the wrong `N` matches nothing (too long) or a
    /// wider namespace than intended (too short), and either way the statement
    /// still parses, still runs, and still reports success.
    #[test]
    fn the_reserved_sweep_matches_the_whole_prefix_and_nothing_shorter() {
        let provisioning = runtime_role_provisioning_sql(
            &fixture_app_id(),
            &fixture_schema(),
            "zs_migrator_fixture",
        )
        .expect("test runtime role name");
        let sql = provisioning.revoke_reserved;
        assert_eq!(RESERVED_SYSTEM_TABLE_PREFIX, "__zeroship_");
        assert!(
            sql.contains("left(c.relname, 11) = '__zeroship_'"),
            "the prefix predicate must carry the prefix's own length: {sql}"
        );
        // Sequences are a separate REVOKE verb; a table-only sweep would leave
        // the journal's identity sequences reachable.
        assert!(
            sql.contains("REVOKE ALL PRIVILEGES ON SEQUENCE %I.%I FROM %I")
                && sql.contains("REVOKE ALL PRIVILEGES ON TABLE %I.%I FROM %I"),
            "both relation kinds must be revoked: {sql}"
        );
        // No name is exempted here. The audit table is re-granted afterwards by
        // its own recipe, which is what makes a NEW writable reserved table have
        // to be added deliberately rather than inherited.
        assert!(
            !sql.contains(AUDIT_UNMASK_TABLE),
            "the sweep must not carve out an exception by name: {sql}"
        );
    }

    #[test]
    fn runtime_provisioning_delegates_only_precreated_narrow_roles() {
        let provisioning = runtime_role_provisioning_sql(
            &fixture_app_id(),
            &fixture_schema(),
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
            "GRANT \"app_app_02xfbOcLMnlN2aR6iBlNi0_role\" \
             TO \"zeroship_worker\" WITH INHERIT FALSE"
        ));
        // Belt and braces against a re-grant that drops the option: a bare
        // `... TO "zeroship_worker";` terminator is the pre-fix statement.
        assert!(
            !sql.contains("TO \"zeroship_worker\";"),
            "an unqualified grant re-opens the ambient union across every app: {sql}"
        );
        // THE JOURNAL SCHEMA IS THE APP'S OWN SCHEMA NOW. It was
        // `app_<hyphenated uuid>` beside a data schema of the bare uuid; a
        // printed app id already carries the `app_` prefix, so the one
        // derivation names one schema for both. The literal below is that
        // schema, NOT the old prefix-on-a-prefix, and it is spelled out so a
        // regression to `format!("app_{}", schema)` - which would emit
        // `app_app_...` and put the journal where the workflow plugin does not
        // look - fails here.
        assert!(sql.contains(
            "CREATE SCHEMA IF NOT EXISTS \"app_02xfbOcLMnlN2aR6iBlNi0\" \
             AUTHORIZATION \"zeroship_workflow_owner\""
        ));
        assert!(
            !sql.contains("\"app_app_02xfbOcLMnlN2aR6iBlNi0\""),
            "a doubled prefix means the journal schema was composed from the \
             schema instead of asked of the seam: {sql}"
        );
        assert!(!sql.contains("CREATE ROLE"));
    }

    /// The refusal is derived from the SCHEMA, so the pair below is
    /// deliberately mismatched: an overlong schema beside the ordinary fixture
    /// tenant. Production always passes a schema derived from the app id it also
    /// passes, and no app id yields a name this long, so the only way to reach
    /// this arm is to decouple the two.
    #[test]
    fn runtime_provisioning_plan_refuses_overlong_role_names() {
        let overlong_schema = "a".repeat(55);
        assert_eq!(
            runtime_role_provisioning_sql(
                &fixture_app_id(),
                &SchemaName::new(&overlong_schema).expect("fixture schema"),
                "zs_migrator_fixture",
            ),
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
        let app_id = fixture_app_id();
        let sql = runtime_role_provisioning_sql(
            &app_id,
            &fixture_schema(),
            "zs_migrator_fixture",
        )
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

    #[test]
    fn incomplete_history_is_a_classified_creator_conflict() {
        let err = ApplyRequestError::IncompleteHistory {
            missing_versions: vec!["mig_missing_a".to_string(), "mig_missing_b".to_string()],
        };
        assert_eq!(
            apply_error_kind(&err),
            (
                ntex::http::StatusCode::CONFLICT,
                "migration_history_incomplete",
            )
        );
        let detail = err.to_string();
        assert!(
            detail.contains("mig_missing_a"),
            "first missing version: {detail}"
        );
        assert!(
            detail.contains("mig_missing_b"),
            "second missing version: {detail}"
        );
    }

    #[test]
    fn history_status_failure_stays_infrastructure() {
        let err = ApplyRequestError::HistoryAttestation(StatusError::Journal(
            zeroship_migrate::JournalError::Backend("fixture read failure".to_string()),
        ));
        assert_eq!(
            apply_error_kind(&err),
            (
                ntex::http::StatusCode::SERVICE_UNAVAILABLE,
                "migration_infrastructure",
            )
        );
    }

    #[test]
    fn invalid_history_manifest_stays_a_creator_fault() {
        let err = ApplyRequestError::HistoryAttestation(StatusError::PlanManifest(
            "invalid dependency fixture".to_string(),
        ));
        assert_eq!(
            apply_error_kind(&err),
            (
                ntex::http::StatusCode::UNPROCESSABLE_ENTITY,
                "migration_invalid",
            )
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
                &AppId::mint(),
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
                &AppId::mint(),
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
/// The scratch schema as the TYPE `provision_runtime_app_role` takes.
///
/// The binding stays a `String` because the other helpers here - `teardown`,
/// `provision_schema_and_migrator`, `provision_audit_unmask_table` - take
/// `&str` and outnumber the typed one several times over. Converting at the
/// one boundary that needs the type is smaller than pushing `.as_str()`
/// through every other call.
///
/// The `expect` is sound rather than optimistic: `scratch_schema` yields a
/// printed app id, and `validate_schema` accepts ASCII alphanumerics, `_`
/// and `-`. A panic here would mean that helper changed shape, which is a
/// fixture bug worth failing loudly on.
fn scratch_schema_name(raw: &str) -> SchemaName {
    SchemaName::new(raw).expect("a scratch schema is a valid schema name")
}

#[cfg(all(test, feature = "live-db-tests"))]
/// The TENANT a scratch schema names, recovered from the schema itself.
///
/// `provision_runtime_app_role` takes both the tenant and the physical schema:
/// the schema for its grants, the tenant for the workflow journal schema. In
/// production those are derived from one id, and recovering one from the other
/// here is what keeps the fixture pair from drifting apart into a combination
/// no deploy could produce. `app_derivation::schema_name` is the identity on the
/// printed id, so this is its inverse and it refuses anything `scratch_schema`
/// did not produce.
fn scratch_app_id(schema: &str) -> AppId {
    AppId::parse(schema).expect("a scratch schema is a printed app id")
}

#[cfg(all(test, feature = "live-db-tests"))]
mod live_audit_unmask_provisioning {
    use super::*;
    use compio_postgres::NoTls;
    use zeroship_migrate_postgres::role::migrator_role_name;

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

    /// A scratch app schema derived from a real minted [`AppId`], because
    /// `provision_migrator` derives the migrator role from it and production
    /// only ever passes an app id. `scratch_app_id` recovers the tenant from
    /// what this returns, so the pair the provisioning call receives is one a
    /// deploy could actually produce.
    fn scratch_schema() -> String {
        app_derivation::schema_name(&AppId::mint())
    }


    fn audit_table_ref(schema: &str) -> String {
        format!("{}.\"__zeroship_audit_unmask\"", quote_ident(schema))
    }

    /// Drop everything a case created, by name. Never a blanket sweep: this
    /// server is shared with other work.
    async fn teardown(admin: &compio_postgres::Client, schema: &str) {
        // ONE SCHEMA, NOT TWO. This also dropped `app_<schema>`, because the
        // workflow journal used to live in its own schema beside the data
        // schema. The journal schema is now the tenant's own schema, so a
        // second drop would name something nothing creates. The assertion
        // makes that a checked fact rather than a remembered one: the day the
        // two derivations diverge, this fails here rather than leaking a
        // schema per run on a shared server.
        assert_eq!(
            crate::provisioning::workflow_journal_schema_name(&scratch_app_id(schema)),
            schema,
            "the journal schema no longer equals the data schema; teardown must drop both"
        );
        let _ = admin
            .batch_execute(&format!(
                "DROP SCHEMA IF EXISTS {} CASCADE;",
                quote_ident(schema),
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
            let _ = admin
                .batch_execute(&format!("DROP OWNED BY {q} CASCADE"))
                .await;
            let _ = admin
                .batch_execute(&format!("DROP ROLE IF EXISTS {q}"))
                .await;
        }
    }

    /// The explicit database-create operation: data schema, then migrator role.
    /// Returns the migrator role name, which is what the apply path passes on to
    /// `provision_runtime_app_role` after it verifies the schema exists.
    async fn provision_schema_and_migrator(
        admin: &compio_postgres::Client,
        schema: &str,
    ) -> String {
        crate::provisioning::provision_database(admin, schema)
            .await
            .expect("create scratch app database");
        migrator_role_name(schema).expect("derive migrator role")
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
                "claimed_actor",
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
        // The five the data plane always supplies must be NOT NULL; the five it
        // may omit must accept a NULL rather than refuse the row. `claimed_actor`
        // is the DB-3 refusal record and is NULL on every ordinary call.
        for (name, nullable) in [
            ("collection", false),
            ("row_pk", false),
            ("column", false),
            ("classification", false),
            ("outcome", false),
            ("actor_id", true),
            ("actor_role", true),
            ("claimed_actor", true),
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
    /// `provision_runtime_app_role` explicitly looks up the audit table and its
    /// owned serial sequence, revokes every additive privilege, then grants only
    /// table INSERT and sequence USAGE. Both lookups are no-ops when the table is
    /// absent. Every arm runs exactly the same production functions against
    /// equivalent scratch schemas; only their order differs.
    ///
    /// ARM C locates the boundary rather than assuming it. `apply_ir_request`
    /// calls `provision_runtime_app_role` TWICE - once before `apply_sealed` and
    /// once after - so the binding constraint is "before the LAST call", not
    /// "before the first". A table created between the two is still reached,
    /// because the second call re-runs the explicit audit recipe.
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
        provision_runtime_app_role(
            &admin,
            &scratch_app_id(&before),
            &scratch_schema_name(&before),
            &migrator_before,
        )
            .await
            .expect("provision runtime role (production order)");

        // ARM B - the inversion, differing in exactly one variable.
        let after = scratch_schema();
        teardown(&admin, &after).await;
        let migrator_after = provision_schema_and_migrator(&admin, &after).await;
        provision_runtime_app_role(
            &admin,
            &scratch_app_id(&after),
            &scratch_schema_name(&after),
            &migrator_after,
        )
            .await
            .expect("provision runtime role (inverted order)");
        provision_audit_unmask_table(&admin, &after)
            .await
            .expect("provision audit table (inverted order)");

        // ARM C - between the apply path's TWO `provision_runtime_app_role`
        // calls. The second one finds the newly created audit objects, so this
        // arm is expected to be REACHABLE.
        let between = scratch_schema();
        teardown(&admin, &between).await;
        let migrator_between = provision_schema_and_migrator(&admin, &between).await;
        provision_runtime_app_role(
            &admin,
            &scratch_app_id(&between),
            &scratch_schema_name(&between),
            &migrator_between,
        )
            .await
            .expect("provision runtime role (first apply-path call)");
        provision_audit_unmask_table(&admin, &between)
            .await
            .expect("provision audit table (between the two calls)");
        provision_runtime_app_role(
            &admin,
            &scratch_app_id(&between),
            &scratch_schema_name(&between),
            &migrator_between,
        )
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
        // THE CONTROL. Same two calls, opposite order, and the explicit lookups
        // now miss both objects. If this arm ever went true, the ordering
        // comment on `audit_unmask_table_sql` would be describing nothing and
        // arm A would be passing for some other reason.
        assert!(
            !probe(&admin, &insert_priv(&after)).await,
            "a table created AFTER the explicit audit recipe must NOT be reachable - \
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
        // that removed the apply path's SECOND call fails here visibly.
        assert!(
            probe(&admin, &insert_priv(&between)).await,
            "the apply path calls provision_runtime_app_role twice; a table \
             created between them is granted by the second call. If this \
             is false, one of those two calls is gone and the ordering docstring \
             on audit_unmask_table_sql needs re-reading"
        );
        assert!(
            probe(&admin, &sequence_priv(&between)).await,
            "the second call must grant the sequence as well"
        );

        teardown(&admin, &before).await;
        teardown(&admin, &after).await;
        teardown(&admin, &between).await;
    }
}

// ---------------------------------------------------------------------------
// Live proof that the app runtime role cannot reach the migration journal
// ---------------------------------------------------------------------------
//
// The journal lives IN the creator's schema, so `GRANT ... ON ALL TABLES IN
// SCHEMA` reaches it. This module proves the sweep that follows takes it back,
// and it proves it by DOING THE WRITE rather than by reading the statement:
// a GRANT is a claim, and only the server can say whether it took. Deleting
// `revoke_runtime_reserved_privileges_sql` from the plan turns these cases red
// by letting a forged `applied` row COMMIT.
//
// The journal's table names come from the ENGINE here - `ensure_journal` is the
// real producer - so a rename in `zeroship-migrate-postgres` shows up as a failed
// expectation instead of a sweep that quietly rules on nothing.
#[cfg(all(test, feature = "live-db-tests"))]
mod live_reserved_journal_privileges {
    use super::*;
    use compio_postgres::NoTls;
    use zeroship_migrate_postgres::role::migrator_role_name;

    /// The five journal tables `ensure_journal` creates, as the engine spells
    /// them.
    ///
    /// Not the doc's list and not this module's guess: the assertion below reads
    /// the catalog after a real bootstrap and compares. `__zeroship_schema_backfills`
    /// is the sixth reserved journal table, created lazily by `run_backfill`
    /// rather than by `ensure_journal`; it is covered by the same prefix, and the
    /// "created after provisioning" arm stands in for it.
    const ENGINE_JOURNAL_TABLES: [&str; 5] = [
        "__zeroship_schema_deploy_recovery",
        "__zeroship_schema_migrations",
        "__zeroship_schema_migrations_inflight",
        "__zeroship_schema_migrations_supersedes",
        "__zeroship_schema_pending_contracts",
    ];

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

    /// Drop everything a case created, by name. Roles are CLUSTER-wide, so a
    /// case that leaves one behind poisons the next run in any database here.
    async fn teardown(admin: &compio_postgres::Client, schema: &str) {
        // ONE SCHEMA, NOT TWO. This also dropped `app_<schema>`, because the
        // workflow journal used to live in its own schema beside the data
        // schema. The journal schema is now the tenant's own schema, so a
        // second drop would name something nothing creates. The assertion
        // makes that a checked fact rather than a remembered one: the day the
        // two derivations diverge, this fails here rather than leaking a
        // schema per run on a shared server.
        assert_eq!(
            crate::provisioning::workflow_journal_schema_name(&scratch_app_id(schema)),
            schema,
            "the journal schema no longer equals the data schema; teardown must drop both"
        );
        let _ = admin
            .batch_execute(&format!(
                "DROP SCHEMA IF EXISTS {} CASCADE;",
                quote_ident(schema),
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
            let _ = admin
                .batch_execute(&format!("DROP OWNED BY {q} CASCADE"))
                .await;
            let _ = admin
                .batch_execute(&format!("DROP ROLE IF EXISTS {q}"))
                .await;
        }
    }

    /// Run `sql` with the connection's role switched to the app runtime role -
    /// the identity creator code executes under - and switch back either way.
    async fn as_runtime_role(
        admin: &compio_postgres::Client,
        schema: &str,
        sql: &str,
    ) -> Result<(), compio_postgres::Error> {
        let role = per_app_role_name(schema).expect("scratch runtime role name");
        admin
            .batch_execute(&format!("SET ROLE {}", quote_ident(&role)))
            .await
            .expect("SET ROLE to the app runtime role");
        let result = admin.batch_execute(sql).await;
        admin
            .batch_execute("RESET ROLE")
            .await
            .expect("RESET ROLE after the probe");
        result
    }

    fn is_insufficient_privilege(err: &compio_postgres::Error) -> bool {
        err.code() == Some(&compio_postgres::error::SqlState::INSUFFICIENT_PRIVILEGE)
    }

    /// A forged terminal journal event, satisfying every CHECK the engine put on
    /// the table. Only the ACL can refuse it.
    fn forge_applied_event(schema: &str) -> String {
        format!(
            "INSERT INTO {}.\"__zeroship_schema_migrations\" \
               (event_kind, version, name, checksum, \"by\", phase, outcome, kind) \
             VALUES ('applied', 'mig_forged', 'forged', 'deadbeef', 'creator-code', \
                     'completed', 'applied', 'apply')",
            quote_ident(schema)
        )
    }

    /// Deleting the two-phase marker is the OTHER half: the executor's recovery
    /// path reads it on the next apply, and the inflight table is deliberately
    /// mutable, so no immutability trigger stands between creator code and it.
    fn delete_inflight_marker(schema: &str) -> String {
        format!(
            "DELETE FROM {}.\"__zeroship_schema_migrations_inflight\"",
            quote_ident(schema)
        )
    }

    /// Every reserved relation in the schema, as the catalog has it.
    async fn reserved_tables(admin: &compio_postgres::Client, schema: &str) -> Vec<String> {
        admin
            .query(
                "SELECT c.relname FROM pg_class c \
                   JOIN pg_namespace n ON n.oid = c.relnamespace \
                  WHERE n.nspname = $1 AND c.relkind IN ('r', 'p') \
                    AND left(c.relname, 11) = '__zeroship_' \
                  ORDER BY c.relname",
                &[&schema],
            )
            .await
            .expect("read reserved relations")
            .iter()
            .map(|r| r.get::<_, String>(0))
            .collect()
    }

    async fn has_privilege(
        admin: &compio_postgres::Client,
        schema: &str,
        table: &str,
        privilege: &str,
    ) -> bool {
        let role = per_app_role_name(schema).expect("scratch runtime role name");
        admin
            .query_one_scalar::<bool, _>(
                "SELECT has_table_privilege($1, format('%I.%I', $2::text, $3::text), $4)",
                &[&role, &schema, &table, &privilege],
            )
            .await
            .expect("read has_table_privilege")
    }

    /// The full production sequence: create the database, bootstrap the engine's
    /// journal, create the audit table, provision the runtime role. Returns the
    /// migrator role name.
    async fn provision_through_the_apply_path(
        admin: &compio_postgres::Client,
        schema: &str,
    ) -> String {
        crate::provisioning::provision_database(admin, schema)
            .await
            .expect("create scratch app database");
        let (exec_cfg, migrator) =
            migrator_executor_config(schema).expect("derive the migrator executor config");
        // The real producer. `attest_complete_history` calls exactly this before
        // the apply path's first `provision_runtime_app_role`.
        let session = CompioPgSession::connect(&test_dsn())
            .await
            .expect("open an engine session");
        PostgresBackend::new_generic(&session)
            .ensure_journal(&exec_cfg)
            .await
            .expect("bootstrap the engine journal");
        provision_audit_unmask_table(admin, schema)
            .await
            .expect("provision the audit table");
        provision_runtime_app_role(
            admin,
            &scratch_app_id(schema),
            &scratch_schema_name(schema),
            &migrator,
        )
            .await
            .expect("provision the runtime role");
        migrator
    }

    /// THE REGRESSION. Creator code cannot read, write or delete the migration
    /// journal, and the two writes that would corrupt it are refused BY THE
    /// SERVER.
    ///
    /// The creator-table and audit-table arms are the controls: they differ from
    /// the journal arms in the table's name and nothing else, so a sweep that
    /// over-revoked - or a provisioning call that simply failed - cannot pass
    /// here by making everything unreachable.
    #[compio::test]
    async fn the_runtime_role_cannot_touch_the_migration_journal() {
        let admin = admin_client().await;
        let schema = app_derivation::schema_name(&AppId::mint());
        teardown(&admin, &schema).await;
        let migrator = provision_through_the_apply_path(&admin, &schema).await;

        // A creator table, created by the migrator exactly as an apply would.
        admin
            .batch_execute(&format!(
                "SET ROLE {}; CREATE TABLE {}.notes (id bigint PRIMARY KEY, body text); RESET ROLE",
                quote_ident(&migrator),
                quote_ident(&schema),
            ))
            .await
            .expect("create a creator table as the migrator");
        // A marker for the DELETE probe to aim at, so a pre-fix pass cannot be
        // an empty-table no-op.
        admin
            .batch_execute(&format!(
                "INSERT INTO {}.\"__zeroship_schema_migrations_inflight\" \
                   (version, name, checksum, applied_by) \
                 VALUES ('mig_marker', 'marker', 'cafe', 'migrate-server')",
                quote_ident(&schema)
            ))
            .await
            .expect("seed an inflight marker");
        provision_runtime_app_role(
            &admin,
            &scratch_app_id(&schema),
            &scratch_schema_name(&schema),
            &migrator,
        )
            .await
            .expect("the apply path's second provisioning call");

        // 1. The engine's journal is what we think it is. If this fails, the
        // sweep below may be ruling on a set that no longer contains the ledger.
        let reserved = reserved_tables(&admin, &schema).await;
        let mut expected: Vec<String> = ENGINE_JOURNAL_TABLES
            .iter()
            .map(|s| (*s).to_string())
            .collect();
        expected.push(AUDIT_UNMASK_TABLE.to_string());
        expected.sort();
        assert_eq!(
            reserved, expected,
            "the reserved relations the engine + provisioning actually created"
        );

        // 1b. WHO OWNS THEM, because that is the premise for leaving the
        // `ALTER DEFAULT PRIVILEGES ... ON TABLES` entries alone. Those entries
        // are declared `FOR ROLE <migrator>` and fire only for objects the
        // migrator creates; the engine bootstraps its journal on the migration
        // service's own session instead. If this ever flips, journal tables
        // start arriving pre-granted and the comment on `grants` is wrong.
        let journal_owner: String = admin
            .query_one_scalar(
                "SELECT tableowner FROM pg_tables \
                  WHERE schemaname = $1 AND tablename = '__zeroship_schema_migrations'",
                &[&schema],
            )
            .await
            .expect("read the journal's owner");
        assert_ne!(
            journal_owner, migrator,
            "the engine journal is created by the migration service's own \
             session, not by the migrator role"
        );

        // 2. NO privilege of any kind survives on any journal table.
        for table in ENGINE_JOURNAL_TABLES {
            for privilege in ["SELECT", "INSERT", "UPDATE", "DELETE"] {
                assert!(
                    !has_privilege(&admin, &schema, table, privilege).await,
                    "the app runtime role still holds {privilege} on {table}"
                );
            }
        }

        // 3. THE WRITES, ATTEMPTED FOR REAL. `has_table_privilege` and the
        // executor consult the same ACL, but only this shows the row does not
        // land.
        let forged = as_runtime_role(&admin, &schema, &forge_applied_event(&schema))
            .await
            .expect_err("creator code must not be able to journal an applied event");
        assert!(
            is_insufficient_privilege(&forged),
            "expected permission denied, got {forged}"
        );
        let deleted = as_runtime_role(&admin, &schema, &delete_inflight_marker(&schema))
            .await
            .expect_err("creator code must not be able to clear an inflight marker");
        assert!(
            is_insufficient_privilege(&deleted),
            "expected permission denied, got {deleted}"
        );
        let journal_rows: i64 = admin
            .query_one_scalar(
                &format!(
                    "SELECT count(*) FROM {}.\"__zeroship_schema_migrations\"",
                    quote_ident(&schema)
                ),
                &[],
            )
            .await
            .expect("count journal rows");
        assert_eq!(journal_rows, 0, "a forged event reached the ledger");
        let marker_rows: i64 = admin
            .query_one_scalar(
                &format!(
                    "SELECT count(*) FROM {}.\"__zeroship_schema_migrations_inflight\"",
                    quote_ident(&schema)
                ),
                &[],
            )
            .await
            .expect("count inflight markers");
        assert_eq!(marker_rows, 1, "the inflight marker was deleted");

        // 4. THE CONTROLS. The sweep is scoped to the reserved namespace, so an
        // ordinary creator table keeps full DML and the audit table keeps its
        // append.
        as_runtime_role(
            &admin,
            &schema,
            &format!(
                "INSERT INTO {}.notes (id, body) VALUES (1, 'hello'); \
                 UPDATE {}.notes SET body = 'edited' WHERE id = 1; \
                 DELETE FROM {}.notes WHERE id = 1",
                quote_ident(&schema),
                quote_ident(&schema),
                quote_ident(&schema),
            ),
        )
        .await
        .expect("a creator table must stay fully writable by the app");
        as_runtime_role(
            &admin,
            &schema,
            &format!(
                "INSERT INTO {}.\"__zeroship_audit_unmask\" \
                   (collection, row_pk, \"column\", classification, outcome) \
                 VALUES ('users', '1', 'ssn', 'pii', 'granted')",
                quote_ident(&schema)
            ),
        )
        .await
        .expect("the unmask audit table is the one reserved table the worker appends to");

        teardown(&admin, &schema).await;
    }

    /// A reserved table born AFTER the runtime role was provisioned is stripped
    /// by the next provisioning call - which the apply path always makes.
    ///
    /// This is the `ALTER DEFAULT PRIVILEGES` arm. Those entries are declared
    /// `FOR ROLE <migrator>`, so this creates the table AS THE MIGRATOR: the one
    /// way a future reserved table could arrive already granted. Arm 1 measures
    /// that it does, which is why the sweep cannot be a one-time cleanup at
    /// database-create time; arm 2 measures that the sweep takes it back.
    #[compio::test]
    async fn a_reserved_table_created_after_provisioning_is_stripped_by_the_next_call() {
        let admin = admin_client().await;
        let schema = app_derivation::schema_name(&AppId::mint());
        teardown(&admin, &schema).await;
        let migrator = provision_through_the_apply_path(&admin, &schema).await;

        // The lazily-created sixth journal table, made the way a migrator-owned
        // one would be.
        admin
            .batch_execute(&format!(
                "SET ROLE {}; \
                 CREATE TABLE {}.\"__zeroship_schema_backfills\" \
                   (backfill_id text PRIMARY KEY, rows_done bigint NOT NULL DEFAULT 0); \
                 RESET ROLE",
                quote_ident(&migrator),
                quote_ident(&schema),
            ))
            .await
            .expect("create a reserved table as the migrator");

        // ARM 1 - the hazard is real: default privileges granted it on creation.
        assert!(
            has_privilege(&admin, &schema, "__zeroship_schema_backfills", "INSERT").await,
            "ALTER DEFAULT PRIVILEGES FOR ROLE <migrator> no longer grants a \
             migrator-created table to the runtime role. If this is false the \
             arm below proves nothing, because there was nothing to strip"
        );

        // ARM 2 - and the apply path's next provisioning call takes it back.
        provision_runtime_app_role(
            &admin,
            &scratch_app_id(&schema),
            &scratch_schema_name(&schema),
            &migrator,
        )
            .await
            .expect("re-provision the runtime role");
        for privilege in ["SELECT", "INSERT", "UPDATE", "DELETE"] {
            assert!(
                !has_privilege(&admin, &schema, "__zeroship_schema_backfills", privilege).await,
                "a reserved table created after provisioning kept {privilege}"
            );
        }

        teardown(&admin, &schema).await;
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
        /// The TENANT the production statement names its workflow journal
        /// schema from. It is not derived from [`Fixture::schema`] and cannot
        /// be: the journal schema comes from the app id through
        /// [`crate::provisioning::workflow_journal_schema_name`], and an
        /// [`AppId`] admits no case prefix.
        app_id: AppId,
    }

    impl Fixture {
        fn new(case: &str) -> Self {
            // Hyphen-free so every name below stays a bare identifier.
            let unique = Uuid::new_v4().to_string().replace('-', "");
            Self {
                case: case.to_string(),
                app_role: format!("{PREFIX}_{case}_{unique}_role"),
                worker: format!("{PREFIX}_{case}_{unique}_worker"),
                schema: format!("{PREFIX}_{case}_{unique}_ns"),
                app_id: AppId::mint(),
            }
        }

        /// The `LIKE` pattern covering every PREFIXED object this case creates.
        ///
        /// It does NOT cover the workflow journal schema - see
        /// [`Fixture::app_id`] and [`teardown`].
        fn like(&self) -> String {
            format!("{PREFIX}_{}_%", self.case)
        }

        /// The workflow journal schema the production statement creates, by the
        /// one derivation rather than a second spelling of it.
        fn journal_schema(&self) -> String {
            crate::provisioning::workflow_journal_schema_name(&self.app_id)
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
    ///
    /// SECOND RESIDUAL, NEW WITH THE TYPED ID. This used to also match
    /// `app\_{like}`, because the journal schema was the case's own schema with
    /// an `app_` prefix. It is now derived from the tenant
    /// ([`Fixture::journal_schema`]) and carries no case prefix, so a case that
    /// PANICS leaves exactly one `app_<base62>` schema behind. [`teardown`]
    /// drops it by name on every run that reaches its end; nothing reaches a
    /// leaked one, because the next `Fixture` mints a different tenant.
    async fn sweep(admin: &compio_postgres::Client, fx: &Fixture) {
        let _ = admin.batch_execute("RESET SESSION AUTHORIZATION").await;
        admin
            .batch_execute(&format!(
                "DO $sweep$
                 DECLARE target text;
                 BEGIN
                   FOR target IN
                     SELECT nspname FROM pg_namespace
                      WHERE nspname LIKE '{like}'
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
    /// [`Fixture::journal_schema`] is the workflow journal schema the SECOND
    /// half of `runtime_dependents_sql` creates. Leaving it behind would
    /// accumulate a schema per run on a shared server, and it is easy to miss
    /// because nothing in these cases mentions the journal. It is asked of the
    /// fixture rather than composed here, so it cannot name a schema the
    /// statement did not create.
    async fn teardown(admin: &compio_postgres::Client, fx: &Fixture) {
        let _ = admin.batch_execute("RESET SESSION AUTHORIZATION").await;
        for schema in [fx.schema.clone(), fx.journal_schema()] {
            let _ = admin
                .batch_execute(&format!(
                    "DROP SCHEMA IF EXISTS {} CASCADE",
                    quote_ident(&schema)
                ))
                .await;
        }
        for role in [&fx.worker, &fx.app_role] {
            let q = quote_ident(role);
            let _ = admin
                .batch_execute(&format!("DROP OWNED BY {q} CASCADE"))
                .await;
            let _ = admin
                .batch_execute(&format!("DROP ROLE IF EXISTS {q}"))
                .await;
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
        let sql = runtime_dependents_sql_for_role(&fx.app_id, &fx.app_role);
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
    async fn inherit_options(admin: &compio_postgres::Client, fx: &Fixture) -> Vec<bool> {
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
            .batch_execute(&format!("ALTER ROLE {} NOINHERIT", quote_ident(&fx.worker)))
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
