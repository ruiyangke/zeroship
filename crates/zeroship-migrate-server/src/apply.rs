use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;
use zeroship_core::database_derivation;
use zeroship_core::database_role::DatabaseCapability;
use zeroship_core::schema_name::SchemaName;
use zeroship_core::DatabaseId;
use zeroship_id::UserId;
use zeroship_migrate::apply::journal::DeployRecoveryScope;
use zeroship_migrate::{
    resolve_create_table_policy, Approval, ApprovalScope, DeclarativeApplyError, EngineError,
    ExecutorConfig, GuardConfig, IrAuthor, LiveSchema, LockMode, LoweredArtifact, MigrationBackend,
    MigrationEngine, MigrationIr, PlanStatusManifest, SealError, SealedPolicy, StatusError,
};
// PG-shaped surfaces live in the vendor crate: this PostgreSQL host names
// PostgreSQL directly rather than through a facade re-export.
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

use crate::capability_grants::{grant_capability_columns, CapabilityGrantError};
use crate::policy::{
    confined_guard_policy_for_schema, CreatorPolicyDraft, EffectivePolicy, ManagedPolicyConfig,
    ManagedPolicyError, SealVerifier,
};
use crate::provisioning::{
    grant_audit_unmask_to_capabilities, migrator_executor_config_for_role,
    provision_audit_unmask_table, provision_migrator, ProvisionRoleError,
};
use crate::publication::{reconcile_database_publication, PublicationError};

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ApplyMigrationsRequest {
    pub kind: ApplyKind,
    /// sha256 of the `schema.runtime.json` the SAME build emitted from these
    /// documents, lowercase hex.
    ///
    /// The fold calls `genArtifacts` ONCE, and both the written
    /// `schema.runtime.json` and the recorded documents in this request come out
    /// of that single reply, so the hash names the descriptor that corresponds
    /// to exactly this document set.
    ///
    /// NOTHING ON THE DEPLOY PATH COMPARES IT, and that is the design rather
    /// than an omission. `admit_bindings`
    /// (`crates/zeroship-control/src/publication/catalog.rs`) admits a deploy on
    /// bindings alone, because a descriptor comparison is an equality test and
    /// equality couples every app on a shared database to every other: one
    /// app's migration, a purely additive one included, would invalidate the
    /// build of every co-tenant while all of them kept running correctly. The
    /// check that belongs in its place is a SUBSET test at isolate build in the
    /// worker - refuse when the database lacks something the app requires, say
    /// nothing when it has grown what the app does not use.
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
    #[error("migration database connect: {0}")]
    Connect(compio_postgres::Error),
    #[error("inspect migration database schema: {0}")]
    InspectSchema(compio_postgres::Error),
    /// The derived physical schema name was invalid.
    #[error("database schema name {schema:?} is not a legal identifier: {reason}")]
    SchemaName { schema: String, reason: String },
    /// The database has no schema on this cluster.
    ///
    /// Control declares a database at `provisioning` and the per-cluster
    /// reconciler creates its schema and roles; nothing on the apply path may
    /// create either, because a schema this service invented would be owned by
    /// a role no binding inherits. So the answer is to wait for convergence,
    /// not to retry with a different request.
    ///
    /// Rendered through `as_str` rather than `Display`: [`DatabaseId`]
    /// deliberately implements no `Display`, so every place an id becomes text
    /// is greppable.
    #[error(
        "database {} has no schema on this cluster yet; its reconciler has not converged it",
        database_id.as_str()
    )]
    DatabaseNotCreated { database_id: DatabaseId },
    #[error("migration role provision: {0}")]
    ProvisionRole(#[from] ProvisionRoleError),
    #[error("unmask audit table provision: {0}")]
    ProvisionAuditUnmask(compio_postgres::Error),
    #[error("app publication provision: {0}")]
    ProvisionPublication(#[from] PublicationError),
    #[error("capability column grants: {0}")]
    GrantCapabilityColumns(#[from] CapabilityGrantError),
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

/// Apply a frozen `.ir.json` bundle into the schema of the database it names.
///
/// THERE IS ONE PATH. The ceiling the creator runs under is what bounds them, and
/// the engine still refuses a destructive step it was not handed an [`Approval`] for.
/// THE TARGET IS THE DATABASE, and nothing else identifies it. An app is not a
/// parameter of a migration: several apps may reach one database and a database
/// may have none bound at all, so an app id here could only be arbitrary.
pub async fn apply_ir_documents(
    provision_dsn: &str,
    tmp_root: &Path,
    database_id: &DatabaseId,
    request: &ApplyMigrationsRequest,
    policy_config: &ManagedPolicyConfig,
    principal_id: &UserId,
) -> Result<ApplyMigrationsResponse, ApplyRequestError> {
    validate_request_shape(request)?;
    let migration_id = Uuid::now_v7();

    match request.kind {
        ApplyKind::Ir => {}
    }
    if request.documents.is_empty() {
        return Err(ApplyRequestError::Empty);
    }

    // THE SERVICE'S ONE DATABASE-TO-SCHEMA DERIVATION. Everything downstream
    // that means "the physical schema" takes the [`SchemaName`].
    let schema_text = database_derivation::schema_name(database_id);
    let schema = SchemaName::new(&schema_text).map_err(|reason| ApplyRequestError::SchemaName {
        schema: schema_text.clone(),
        reason: reason.to_string(),
    })?;

    // THE CEILING IS BOUND TO THAT SCHEMA, which is why it is composed here and
    // not before the derivation: every namespace-scoped grant in it names the
    // schema this apply writes, and a ceiling bound to any other name grants
    // nothing here and refuses every creator statement as out-of-scope.
    let policy = resolve_apply_policy(&schema, request, policy_config)?;

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
            database_id: database_id.clone(),
        });
    }

    // The existence fence is above all filesystem, role, audit, lock, and
    // ledger side effects. A refused apply therefore leaves no database state
    // for a retry or deploy gate to mistake for a completed lifecycle step.
    let dir = write_ir_documents(tmp_root, request)?;
    // The executor re-vets rendered SQL at apply time from its own policy, so it must
    // carry the same no-inject confined guard charter as guarded lower. The composed
    // inject-bearing policy remains separate and is passed explicitly to shape
    // resolution and `IrAuthor` below.
    // SCHEMA, still spelled `&str`. `migrator_executor_config_for_role`,
    // `provision_audit_unmask_table` and `prepare_ir_documents` all mean the
    // physical schema, and typing them reaches the engine`s `ExecutorConfig`,
    // which takes owned `String`s. The downgrade is explicit and greppable
    // (`schema.as_str()`) rather than a `&str` that either identity satisfies.
    //
    // THE OWNER IS THE RECONCILER'S ROLE, NOT ONE DERIVED HERE. The cluster
    // reconciler minted `zs_db_<dbs>_mig` and handed it this schema; an apply
    // running as anything else would either be refused by the schema's owner or
    // - worse, with `CREATEROLE` in hand - take ownership away from the role
    // the reconciler re-asserts on its next pass.
    let migrator = database_derivation::migrator_role_name(database_id).map_err(|reason| {
        ApplyRequestError::ProvisionRole(ProvisionRoleError::BadRoleName(reason.to_string()))
    })?;
    let exec_cfg = migrator_executor_config_for_role(schema.as_str(), &migrator);
    // THE MIGRATION JOURNAL LIVES IN THE DATABASE'S OWN SCHEMA. `ExecutorConfig::new`
    // derives `meta_schema` as `<project_schema>_migrations`; this host points it at
    // the project schema itself, so the engine writes
    // `"db_<dbs>".__zeroship_schema_migrations` and its five siblings. The record
    // of what ran belongs to the tenant whose schema it describes.
    //
    // THE `__zeroship_` PREFIX IS WHAT MAKES THIS SAFE, and it is not decoration.
    // The engine bootstraps its journal with `CREATE TABLE IF NOT EXISTS` and its
    // table names are literals - only the schema is interpolated - so a creator
    // declaring a table called `schema_migrations` in their own schema would have it
    // silently ADOPTED as the journal. The prefix moves the names into a namespace
    // creator-declared collections are refused from.
    provision_migrator(session.client(), &exec_cfg).await?;
    provision_audit_unmask_table(session.client(), schema.as_str())
        .await
        .map_err(ApplyRequestError::ProvisionAuditUnmask)?;
    // The audit table is reached by the SESSION ROLE a binding narrows to,
    // which inherits one of this database's two capability roles. Without this
    // grant the table exists and no bound session can write it, so every
    // unmask would fail on the audit write rather than on the unmask.
    grant_audit_unmask_to_capabilities(
        session.client(),
        schema.as_str(),
        &database_derivation::capability_role_name(database_id, DatabaseCapability::ReadWrite)
            .map_err(|reason| {
                ApplyRequestError::ProvisionRole(ProvisionRoleError::BadRoleName(
                    reason.to_string(),
                ))
            })?,
        &database_derivation::capability_role_name(database_id, DatabaseCapability::ReadOnly)
            .map_err(|reason| {
                ApplyRequestError::ProvisionRole(ProvisionRoleError::BadRoleName(
                    reason.to_string(),
                ))
            })?,
    )
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
            prepare_ir_documents(&session, &exec_cfg, schema.as_str(), dir.path(), &policy).await?;
        attest_complete_history(&backend, &exec_cfg, &prepared).await?;

        let apply_result = run_apply(
            &backend,
            policy_config,
            &policy,
            database_id,
            &schema,
            &prepared,
            &exec_cfg,
            principal_id,
            session.client(),
        )
        .await;

        match apply_result {
            Ok(outcome) => Ok(ApplyMigrationsResponse {
                migration_id,
                applied: outcome.applied,
                skipped: outcome.skipped,
                pending_contract: outcome.pending_contract,
            }),
            Err(err) => Err(err),
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
    backend: &PostgresBackend<'_, CompioPgSession>,
    policy_config: &ManagedPolicyConfig,
    apply_policy: &EffectivePolicy,
    database_id: &DatabaseId,
    schema: &SchemaName,
    prepared: &[PreparedIrDocument],
    exec_cfg: &ExecutorConfig,
    principal_id: &UserId,
    admin: &compio_postgres::Client,
) -> Result<SealedApplyOutcome, ApplyRequestError> {
    // (d) POLICY: seal the effective policy with the zeroship-migrate-policy HMAC so
    // the apply carries an authenticated, ceiling-stamped integrity token.
    // Pre-launch: stored seals don't matter - this seal is minted+verified in-process
    // for tamper-detection. This sealed policy drives managed shape/lower; the
    // rendered-DDL guard is the fixed schema-bound no-inject confined charter.
    let sealed_policy = policy_config.seal_effective_for_app(apply_policy.clone())?;
    tracing::debug!(
        database_id = database_id.as_str(),
        schema = %schema.as_str(),
        ceiling_id = %sealed_policy.ceiling_id,
        ceiling_version = sealed_policy.ceiling_version,
        "migrate-server: applying IR under sealed managed migration policy"
    );
    let applied_by = format!("migrate-server:{}", principal_id.as_str());
    // NO SCHEMA-WIDE RUNTIME ROLE IS ESTABLISHED HERE, and its absence is the
    // change rather than a gap. A role carrying `SELECT, INSERT, UPDATE,
    // DELETE ON ALL TABLES IN SCHEMA` and granted to the ONE shared worker
    // login would be assumable by every app that login serves - including
    // every co-tenant of this database - which is precisely the reach
    // `GRANT ... WITH SET FALSE` on the binding-to-database edge exists to
    // deny. What an app may read and write on this database is carried by the
    // two capability roles the reconciler minted, narrowed per column by
    // `grant_capability_columns` below.
    let outcome = apply_sealed(
        backend,
        sealed_policy.sealed,
        &sealed_policy.verifier,
        prepared,
        exec_cfg,
        &apply_policy.policy,
        Approval::None,
        &applied_by,
    )
    .await?;
    // The capability grants, over every creator table this database now holds.
    //
    // AFTER the DDL and INSIDE the project lock, which is the strongest
    // bracket this host can put around them. The engine opens and commits its
    // own transaction per lowered unit over the `SqlSession` seam
    // (`zeroship_migrate_postgres::backend::session::apply_transactional`), and
    // that seam carries no host hook, so there is no way to emit a grant in the
    // same transaction as the statement that created the table. The window a
    // separate transaction opens is a table that exists and is unreachable,
    // which is the direction that fails closed, and the emission states a
    // desired state rather than a delta - so a crash inside the window is
    // repaired by the next apply rather than leaving a partial ACL.
    grant_capability_columns(admin, database_id).await?;
    // The publication reconciliation takes the DATABASE: the object is the
    // datastore's one shared publication and the membership being edited is the
    // set of relations in this database's schema.
    reconcile_database_publication(admin, database_id).await?;
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

/// Discover `*.ir.json` files in a directory, deterministically ordered by path.
// `IrApplyError` is a large error because it carries the vendored engine's
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

/// Live facts the PG `.ir.json` apply loop advances between files.
struct PostgresIrApplyState {
    registry: BTreeMap<String, String>,
    live_schema: LiveSchema,
}

/// Seed the PG IR apply state from the live project schema. Introspects the
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
/// POLICY ARRIVES IN THE ARTIFACT, NOT THE DATABASE. A migration policy is declared
/// in the creator's repository, folded at build time and shipped with the request,
/// exactly like the mask policy. A policy nothing can mutate at runtime is one fewer
/// thing to authenticate.
///
/// See the `write_ir_documents` allow below - same `ApplyRequestError` size, same
/// rationale. It surfaces here because `clippy::result_large_err` does not fire
/// through a future, and this is where the function becomes synchronous.
#[allow(clippy::result_large_err)]
fn resolve_apply_policy(
    schema: &SchemaName,
    request: &ApplyMigrationsRequest,
    policy_config: &ManagedPolicyConfig,
) -> Result<EffectivePolicy, ApplyRequestError> {
    let Some(policy) = request.policy.as_ref() else {
        return Ok(policy_config.compose_effective_for_schema(schema.as_str(), None, None)?);
    };
    let draft = CreatorPolicyDraft {
        filename: policy.filename.as_str(),
        body: policy.body.as_str(),
    };
    let parsed = policy_config.parse_draft(&draft)?;
    Ok(policy_config.compose_effective_for_schema(schema.as_str(), None, Some(&parsed))?)
}

/// Lower every document under the guard, refuse a denied plan, and retain the
/// exact artifacts for journal attestation and apply.
///
/// A bundle whose LAST document is denied must be refused before the DDL of the
/// earlier documents commits, so preparation walks the whole set first.
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
/// to the same exact database schema as the inject-bearing policy used during lower.
fn guard_config_for_managed(schema: &str) -> GuardConfig {
    GuardConfig::from_policy(guard_policy_for_managed(schema), POSTGRES, schema)
}

fn guard_policy_for_managed(schema: &str) -> PdpPolicy {
    confined_guard_policy_for_schema(schema)
        .expect("embedded no-inject confined guard charter must bind and compose")
}

// `ApplyRequestError` is a large error because it wraps `IrApplyError` /
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
        // Not a creator fault and not retryable: the caller supplied a database
        // id, the service derived a schema name from it, and the derivation
        // produced something PostgreSQL cannot name. That is a platform defect.
        ApplyRequestError::SchemaName { .. } => (
            ntex::http::StatusCode::INTERNAL_SERVER_ERROR,
            "database_schema_name_invalid",
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
        | ApplyRequestError::Connect(_)
        | ApplyRequestError::InspectSchema(_)
        | ApplyRequestError::ProvisionRole(_)
        | ApplyRequestError::ProvisionAuditUnmask(_)
        | ApplyRequestError::ProvisionPublication(_)
        | ApplyRequestError::GrantCapabilityColumns(_)
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

pub(crate) fn quote_ident(ident: &str) -> String {
    format!("\"{}\"", ident.replace('"', "\"\""))
}

pub(crate) fn quote_lit(value: &str) -> String {
    value.replace('\'', "''")
}

/// The ONE login role every worker process connects as.
///
/// Precreated by the platform migrations
/// (`db/migrations-ts/20260702000100_schema_roles_extensions.ts`); this
/// service only grants it membership in each live binding role, which it may
/// assume and inherits none of (`crate::datastore::cluster`).
///
/// Public so end-to-end tests can connect as the production login without
/// copying this login-role literal.
pub const WORKER_ROLE: &str = "zeroship_worker";

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

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
        let principal_id = UserId::mint();
        let err = rt
            .block_on(apply_ir_documents(
                "postgres://unused",
                Path::new("/tmp"),
                &DatabaseId::mint(),
                &request,
                &policy_config,
                &principal_id,
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
        let principal_id = UserId::mint();
        let err = rt
            .block_on(apply_ir_documents(
                "postgres://unused",
                Path::new("/tmp"),
                &DatabaseId::mint(),
                &request,
                &policy_config,
                &principal_id,
            ))
            .expect_err("scalar request rejected before DB connect");
        assert!(matches!(err, ApplyRequestError::InvalidDocument(_)));
    }
}

// ---------------------------------------------------------------------------
// Live provisioning proofs for the unmask audit table
// ---------------------------------------------------------------------------
//
// These ordinary cargo tests own PostgreSQL through the crate's private fixture.
//
// WHAT THESE COVER THAT THE UNIT TESTS ABOVE CANNOT. `audit_unmask_tests` in
// `provisioning.rs` greps the generated string: it proves the DDL SAYS
// `CREATE TABLE IF NOT EXISTS "app"."__zeroship_audit_unmask"`, never that a
// server accepted it or that the table came out with the columns the data
// plane binds. The whole point of moving this DDL out of the worker is a
// privilege change, and a privilege change is only observable against a live
// catalog.
#[cfg(test)]
mod live_audit_unmask_provisioning {
    use super::*;
    use crate::datastore::cluster::{converge_database, drop_database};

    async fn admin_client() -> compio_postgres::Client {
        crate::test_database::connect().await
    }

    fn audit_table_ref(schema: &str) -> String {
        format!("{}.\"__zeroship_audit_unmask\"", quote_ident(schema))
    }

    /// Drop everything a case created, by name. Never a blanket sweep: this
    /// server is shared with other work. `drop_database` is the reconciler's
    /// own teardown, and it removes exactly the schema and the roles
    /// `converge_database` derived from this one database id.
    async fn teardown(admin: &mut compio_postgres::Client, database: &DatabaseId) {
        drop_database(admin, database)
            .await
            .expect("drop the scratch database");
    }

    /// A scratch creator database, converged the way the cluster reconciler
    /// converges one: its schema, the migrator that owns it, and its
    /// capability and unmask roles.
    ///
    /// Minted per case because `pg_authid` is cluster-shared: every one of
    /// those roles is derived from the database id, so two cases on one id
    /// would create and drop each other's roles.
    async fn converge_scratch_database(
        admin: &mut compio_postgres::Client,
    ) -> (DatabaseId, String) {
        let database = DatabaseId::mint();
        converge_database(admin, &database)
            .await
            .expect("converge the scratch database");
        let schema = database_derivation::schema_name(&database);
        (database, schema)
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
        let mut admin = admin_client().await;
        let (database, schema) = converge_scratch_database(&mut admin).await;

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

        // OWNERSHIP: the table is created by the migration service's admin
        // principal, so no role a bound session reaches owns it. A binding
        // inherits one of the database's two capability roles and may assume
        // its unmask role; each of them is an ordinary grantee of this table.
        let owner: String = admin
            .query_one_scalar(
                "SELECT tableowner FROM pg_tables \
                  WHERE schemaname = $1 AND tablename = '__zeroship_audit_unmask'",
                &[&schema],
            )
            .await
            .expect("read table owner");
        for runtime in [
            database_derivation::capability_role_name(&database, DatabaseCapability::ReadWrite),
            database_derivation::capability_role_name(&database, DatabaseCapability::ReadOnly),
            database_derivation::unmask_role_name(&database),
        ] {
            let runtime = runtime.expect("scratch database role name");
            // The control: the name compared against is a role the converge
            // minted, so the inequality below cannot pass over a misspelling.
            assert!(
                probe(
                    &admin,
                    &format!("SELECT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = '{runtime}')"),
                )
                .await,
                "{runtime} must be a role the converged database carries"
            );
            assert_ne!(
                owner, runtime,
                "a role a bound session reaches must NOT own its own audit log"
            );
        }

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

        teardown(&mut admin, &database).await;
    }
}
