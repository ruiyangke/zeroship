//! App lifecycle changes committed with their publication intent.
//!
//! Every function taking a `&Database` performs its reads and writes through
//! that handle. Pass the database a `Database::transaction` callback received,
//! normally through [`transact`], so the app pointer, the deployment row, the
//! lifecycle revision, the intent and the command receipt commit or roll back
//! together. Nothing here performs manager or blob I/O.

use super::command::{
    Acceptance, AcceptanceResult, CommandBinding, DeployCommand, VerifiedDeployment,
    DEPLOY_OPERATION,
};
use super::models::catalog::{
    app_deploy_commands as commands, app_lifecycle_intents as intents,
    app_schema_applies as applies, apps,
};
use std::{future::Future, num::NonZeroUsize, pin::Pin};
use zeroship_core::{
    app_id::AppId, schema_name::SchemaName, workflow_coordination::Revision,
    workflow_jobs::DeploymentId, DeployCommandId,
};
use zeroship_data_orm::{
    binding::DbBinding,
    encryption::ProjectKeySource,
    error::DbError,
    orm::{Database, FromRow, Insertable, UtcInstant},
    ConnectOptions,
};
use zeroship_workflow_manager::deployments::models::app_deploys as deploys;

/// The intent action that selects a deployment for new work.
pub const ACTIVATE: &str = "activate";
/// The intent action that stops calendar generation.
pub const DISABLE: &str = "disable";
/// An intent the manager has not acknowledged.
pub const PENDING: &str = "pending";
/// An intent confirmed by the manager's exact receipt.
pub const ACKNOWLEDGED: &str = "acknowledged";

/// Catalog refusals and failures. Database diagnostics stay in the
/// `Database` variant; callers map them to generic infrastructure errors.
#[derive(Debug, thiserror::Error)]
pub enum CatalogError {
    #[error("app not found")]
    AppAbsent,
    #[error("the deploy command id is bound to a different deploy")]
    CommandConflict,
    #[error("the deployment's runtime schema descriptor is not the app's applied schema")]
    SchemaNotApplied {
        descriptor_sha256: Option<String>,
        applied_sha256: Option<String>,
    },
    #[error("deployment retention has closed activation")]
    DeploymentReclaimed,
    #[error("a schema apply is in progress")]
    ApplyInProgress,
    #[error("the retained deployment cannot be restored: {0}")]
    InvalidRetained(String),
    #[error("the app's lifecycle revision is exhausted")]
    RevisionExhausted,
    #[error("invalid control catalog metadata: {0}")]
    Storage(&'static str),
    #[error("control catalog database operation failed: {0}")]
    Database(DbError),
}

impl From<DbError> for CatalogError {
    fn from(error: DbError) -> Self {
        Self::Database(error)
    }
}

/// The lifecycle effect of an archive or restore.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transition {
    /// The app was already in the requested state; no revision was allocated.
    Unchanged,
    /// Restored without a staged deployment to activate.
    Restored,
    /// The transition committed an intent at this lifecycle revision.
    Published(Revision),
}

/// The session name catalog connections announce.
///
/// It tells them apart from Control's other connections in
/// `pg_stat_activity`. A name the database URL already sets, or already
/// offers as a fallback, is left in place.
pub const APPLICATION_NAME: &str = "zeroship-control-catalog";

/// Open a native ORM database on the Control catalog holding one session.
///
/// The database belongs to the calling compio thread, which admits one
/// top-level transaction at a time, so a second session would never be in
/// use; [`super::Catalog`] scales the process by opening one of these per
/// thread.
///
/// # Errors
/// Reports invalid model declarations and unreachable storage.
pub async fn connect(url: &str) -> Result<Database, DbError> {
    Database::connect(
        DbBinding::new(
            "platform",
            "control-catalog",
            SchemaName::new("zeroship")
                .map_err(|_| DbError::config("invalid_catalog_schema", "invalid catalog schema"))?,
        ),
        ConnectOptions::new(named(url), ProjectKeySource::unavailable())
            .max_connections(NonZeroUsize::MIN)
            .connection_authority(),
        super::models::collections()?,
    )
    .await
}

/// `url` offering [`APPLICATION_NAME`] as its session name. Only a URL that
/// parses and names no session of its own is extended, and the original text
/// is kept so its encoding reaches the driver unchanged.
fn named(url: &str) -> String {
    const KEYS: [&str; 2] = ["application_name", "fallback_application_name"];
    let Ok(parsed) = url::Url::parse(url) else {
        return url.to_owned();
    };
    if parsed.fragment().is_some() || parsed.query_pairs().any(|(key, _)| KEYS.contains(&&*key)) {
        return url.to_owned();
    }
    let separator = match parsed.query() {
        None => "?",
        Some("") => "",
        Some(_) => "&",
    };
    format!("{url}{separator}fallback_application_name={APPLICATION_NAME}")
}

/// Run `body` in one transaction. A catalog refusal returned by the callback
/// rolls back every write it staged and is returned unchanged; an ORM
/// settlement failure takes precedence.
///
/// # Errors
/// Returns the callback's refusal or the transaction's database failure.
pub async fn transact<T, F, Fut>(database: &Database, body: F) -> Result<T, CatalogError>
where
    F: FnOnce(Database) -> Fut,
    Fut: Future<Output = Result<T, CatalogError>>,
{
    const REFUSED: &str = "control_catalog_refused";
    let mut refusal = None;
    let saved = &mut refusal;
    let result = database
        .transaction(|tx| {
            // Erase the callback's layout before handing it to the ORM.
            let callback: Pin<Box<dyn Future<Output = Result<T, DbError>> + '_>> =
                Box::pin(async move {
                    match body(tx).await {
                        Ok(value) => Ok(value),
                        Err(CatalogError::Database(error)) => Err(error),
                        Err(error) => {
                            *saved = Some(error);
                            Err(DbError::validation(REFUSED, "control catalog refused"))
                        }
                    }
                });
            callback
        })
        .await;
    match result {
        Ok(value) => Ok(value),
        Err(DbError::ValidationFailed { code: REFUSED, .. }) => {
            Err(refusal.unwrap_or(CatalogError::Storage("lost catalog refusal")))
        }
        Err(error) => Err(CatalogError::Database(error)),
    }
}

#[derive(FromRow)]
#[orm(entity = apps)]
struct AppRow {
    deploy_hash: Option<String>,
    manifest_json: Option<String>,
    lifecycle_revision: i64,
    archived_at: Option<UtcInstant>,
}

#[derive(FromRow)]
#[orm(entity = applies)]
struct AppliedSchema {
    descriptor_sha256: String,
}

#[derive(FromRow)]
#[orm(entity = commands)]
struct ReceiptRow {
    app_id: String,
    actor_id: Option<String>,
    operation: String,
    content_type: String,
    archive_sha256: String,
    deploy_id: String,
    deploy_hash: String,
    lifecycle_revision: Option<i64>,
    result: String,
}

#[derive(Insertable)]
#[orm(entity = commands)]
struct NewReceipt<'a> {
    id: &'a str,
    app_id: &'a str,
    actor_id: Option<&'a str>,
    operation: &'a str,
    content_type: &'a str,
    archive_sha256: &'a str,
    deploy_id: &'a str,
    deploy_hash: &'a str,
    lifecycle_revision: Option<i64>,
    result: &'a str,
    created_at: UtcInstant,
}

#[derive(FromRow)]
#[orm(entity = commands)]
struct ReceiptId {
    id: String,
}

#[derive(FromRow)]
#[orm(entity = deploys)]
struct DeploymentRow {
    id: String,
    retention_state: String,
}

#[derive(Insertable)]
#[orm(entity = deploys)]
struct NewDeployment<'a> {
    id: &'a str,
    app_id: &'a str,
    deploy_hash: &'a str,
    manifest_json: &'a str,
    created_at: UtcInstant,
    activated_at: Option<UtcInstant>,
}

#[derive(Insertable)]
#[orm(entity = intents)]
struct NewIntent<'a> {
    id: &'a str,
    app_id: &'a str,
    revision: i64,
    action: &'a str,
    deploy_id: Option<&'a str>,
    registration: Option<&'a str>,
    state: &'a str,
    created_at: UtcInstant,
}

#[derive(FromRow)]
#[orm(entity = intents)]
struct IntentId {
    id: String,
}

impl ReceiptRow {
    /// Answer an exact retry with the stored result. Any difference in the
    /// binding is a conflict that reveals nothing about the stored command.
    fn answer(self, binding: &CommandBinding) -> Result<AcceptanceResult, CatalogError> {
        let bound = self.app_id == binding.app.as_str()
            && self.actor_id.as_deref() == Some(binding.actor.as_str())
            && self.operation == DEPLOY_OPERATION
            && self.content_type == binding.content_type
            && self.archive_sha256 == binding.archive_sha256;
        if !bound {
            return Err(CatalogError::CommandConflict);
        }
        let result: AcceptanceResult = serde_json::from_str(&self.result)
            .map_err(|_| CatalogError::Storage("deploy receipt result"))?;
        if result.command_id != binding.id
            || result.deploy_id.as_str() != self.deploy_id
            || result.deploy_hash != self.deploy_hash
            || result.lifecycle_revision.map(Revision::get) != self.lifecycle_revision
        {
            return Err(CatalogError::Storage("deploy receipt binding"));
        }
        Ok(result)
    }
}

/// Accept a deploy command in the caller's transaction.
///
/// Recheck its receipt, admit the schema, select the deployment, move the app
/// pointer, publish an activation for an active app and record the receipt.
/// An exact retry replays the stored result before any other check.
///
/// # Errors
/// Refuses absent or deleted apps, receipt conflicts, schema admission,
/// reclaimed deployments and exhausted revisions; reports storage failures.
pub async fn accept(
    tx: &Database,
    command: &DeployCommand,
    now: UtcInstant,
) -> Result<Acceptance, CatalogError> {
    let binding = &command.binding;
    let app = &binding.app;
    let state = lock_live_app(tx, app)
        .await?
        .ok_or(CatalogError::AppAbsent)?;
    if let Some(receipt) = find_receipt(tx, &binding.id).await? {
        return receipt.answer(binding).map(Acceptance::Replayed);
    }
    admit_schema(tx, app, command.deployment.descriptor_sha256()).await?;
    let deployment = acquire_deployment(tx, app, &command.deployment, now).await?;
    changed(
        tx.entity::<apps::Entity>()?
            .update_many(
                apps::id.eq(app.as_str())?,
                apps::deploy_hash
                    .set(Some(command.deployment.hash()))?
                    .and(apps::manifest_json.set(Some(command.deployment.manifest_json()))?)?
                    .and(apps::updated_at.set(now)?)?,
            )
            .await?,
    )?;
    let lifecycle_revision = if state.archived_at.is_none() {
        let revision = allocate_revision(tx, app, state.lifecycle_revision).await?;
        let registration =
            serde_json::to_string(&command.deployment.registration(app, &deployment))
                .map_err(|_| CatalogError::Storage("schedule registration"))?;
        insert_intent(
            tx,
            app,
            revision,
            ACTIVATE,
            Some((&deployment, &registration)),
            now,
        )
        .await?;
        Some(revision)
    } else {
        None
    };
    let result = AcceptanceResult {
        command_id: binding.id.clone(),
        deploy_id: deployment,
        deploy_hash: command.deployment.hash().to_owned(),
        blobs_uploaded: command.blobs_uploaded,
        blobs_deduped: command.blobs_deduped,
        lifecycle_revision,
    };
    let encoded =
        serde_json::to_string(&result).map_err(|_| CatalogError::Storage("deploy result"))?;
    let inserted = tx
        .entity::<commands::Entity>()?
        .insert::<_, ReceiptId>(NewReceipt {
            id: binding.id.as_str(),
            app_id: app.as_str(),
            actor_id: Some(binding.actor.as_str()),
            operation: DEPLOY_OPERATION,
            content_type: binding.content_type,
            archive_sha256: &binding.archive_sha256,
            deploy_id: result.deploy_id.as_str(),
            deploy_hash: &result.deploy_hash,
            lifecycle_revision: lifecycle_revision.map(Revision::get),
            result: &encoded,
            created_at: now,
        })
        .await;
    match inserted {
        Ok(row) if row.id == binding.id.as_str() => Ok(Acceptance::Accepted(result)),
        Ok(_) => Err(CatalogError::Storage("deploy receipt identity")),
        // Another app already claimed this command id; its app lock did not
        // serialize with this one, so the unique key decides.
        Err(
            DbError::UniqueViolation { .. }
            | DbError::SchemaRefused {
                code: "unique_violation",
                ..
            },
        ) => Err(CatalogError::CommandConflict),
        Err(error) => Err(error.into()),
    }
}

/// Look up a command's receipt before ingest so an exact retry performs no
/// blob I/O. [`accept`] rechecks under the app lock.
///
/// # Errors
/// Refuses absent or deleted apps and receipt conflicts.
pub async fn lookup(
    tx: &Database,
    binding: &CommandBinding,
) -> Result<Option<AcceptanceResult>, CatalogError> {
    let live = tx
        .entity::<apps::Entity>()?
        .query()
        .filter(
            apps::id
                .eq(binding.app.as_str())?
                .and(apps::deleted_at.is_null()),
        )
        .exists()
        .await?;
    if !live {
        return Err(CatalogError::AppAbsent);
    }
    find_receipt(tx, &binding.id)
        .await?
        .map(|receipt| receipt.answer(binding))
        .transpose()
}

/// Archive an active app: record the marker and a disable intent at the next
/// lifecycle revision. Archiving an archived app changes nothing.
///
/// # Errors
/// Reports exhausted revisions and storage failures. `Ok(None)` means the app
/// is absent or deleted.
pub async fn archive(
    tx: &Database,
    app: &AppId,
    now: UtcInstant,
) -> Result<Option<Transition>, CatalogError> {
    let Some(state) = lock_live_app(tx, app).await? else {
        return Ok(None);
    };
    if state.archived_at.is_some() {
        return Ok(Some(Transition::Unchanged));
    }
    let revision = allocate_revision(tx, app, state.lifecycle_revision).await?;
    changed(
        tx.entity::<apps::Entity>()?
            .update_many(
                apps::id.eq(app.as_str())?.and(apps::archived_at.is_null()),
                apps::archived_at
                    .set(Some(now))?
                    .and(apps::updated_at.set(now)?)?,
            )
            .await?,
    )?;
    insert_intent(tx, app, revision, DISABLE, None, now).await?;
    Ok(Some(Transition::Published(revision)))
}

/// Restore an archived app after the existing migration checks, then publish
/// a fresh activation of its staged deployment. Restoring an active app, or an
/// app with no staged deployment, publishes nothing.
///
/// # Errors
/// Refuses an apply in progress, an invalid retained manifest, schema
/// admission and a reclaimed deployment; reports storage failures. `Ok(None)`
/// means the app is absent or deleted.
pub async fn restore(
    tx: &Database,
    app: &AppId,
    now: UtcInstant,
) -> Result<Option<Transition>, CatalogError> {
    let Some(state) = lock_live_app(tx, app).await? else {
        return Ok(None);
    };
    if state.archived_at.is_none() {
        return Ok(Some(Transition::Unchanged));
    }
    let applying = tx
        .entity::<applies::Entity>()?
        .query()
        .filter(
            applies::app_id
                .eq(app.as_str())?
                .and(applies::status.eq("submitted")?),
        )
        .exists()
        .await?;
    if applying {
        return Err(CatalogError::ApplyInProgress);
    }
    let staged = match (state.deploy_hash, state.manifest_json) {
        (Some(hash), Some(manifest)) => Some(
            VerifiedDeployment::verify(manifest, hash)
                .map_err(|error| CatalogError::InvalidRetained(error.to_string()))?,
        ),
        (None, None) => None,
        _ => return Err(CatalogError::Storage("app pointer without its manifest")),
    };
    admit_schema(
        tx,
        app,
        staged
            .as_ref()
            .and_then(VerifiedDeployment::descriptor_sha256),
    )
    .await?;
    changed(
        tx.entity::<apps::Entity>()?
            .update_many(
                apps::id
                    .eq(app.as_str())?
                    .and(apps::archived_at.is_not_null()),
                apps::archived_at
                    .set(None::<UtcInstant>)?
                    .and(apps::updated_at.set(now)?)?,
            )
            .await?,
    )?;
    let Some(staged) = staged else {
        return Ok(Some(Transition::Restored));
    };
    let deployment =
        existing_deployment(tx, app, staged.hash())
            .await?
            .ok_or(CatalogError::Storage(
                "staged deployment is not in the catalog",
            ))?;
    let revision = allocate_revision(tx, app, state.lifecycle_revision).await?;
    let registration = serde_json::to_string(&staged.registration(app, &deployment))
        .map_err(|_| CatalogError::Storage("schedule registration"))?;
    insert_intent(
        tx,
        app,
        revision,
        ACTIVATE,
        Some((&deployment, &registration)),
        now,
    )
    .await?;
    Ok(Some(Transition::Published(revision)))
}

/// Take the app row lock through a no-op update, the same lock the collector
/// takes before fencing a deployment. A deleted app is absent.
async fn lock_live_app(tx: &Database, app: &AppId) -> Result<Option<AppRow>, CatalogError> {
    let locked = tx
        .entity::<apps::Entity>()?
        .update_many(
            apps::id.eq(app.as_str())?.and(apps::deleted_at.is_null()),
            apps::lifecycle_revision.increment(0_i64)?,
        )
        .await?;
    match locked {
        0 => return Ok(None),
        1 => {}
        _ => return Err(CatalogError::Storage("app identity is not unique")),
    }
    let state = tx
        .entity::<apps::Entity>()?
        .query()
        .filter(apps::id.eq(app.as_str())?)
        .first::<AppRow>()
        .await?
        .ok_or(CatalogError::Storage("locked app row vanished"))?;
    if state.lifecycle_revision < 0 {
        return Err(CatalogError::Storage("negative lifecycle revision"));
    }
    Ok(Some(state))
}

async fn find_receipt(
    tx: &Database,
    id: &DeployCommandId,
) -> Result<Option<ReceiptRow>, CatalogError> {
    Ok(tx
        .entity::<commands::Entity>()?
        .query()
        .filter(commands::id.eq(id.as_str())?)
        .first::<ReceiptRow>()
        .await?)
}

/// The deploy precondition: a descriptor must equal the one recorded on the
/// app's newest applied migration, and a descriptor-less artifact is admitted
/// only when the app has no applied schema. Newest, not any: a rollback across
/// a migration boundary must not pass because an older descriptor was once
/// applied.
async fn admit_schema(
    tx: &Database,
    app: &AppId,
    descriptor_sha256: Option<&str>,
) -> Result<(), CatalogError> {
    let applied = tx
        .entity::<applies::Entity>()?
        .query()
        .filter(
            applies::app_id
                .eq(app.as_str())?
                .and(applies::status.eq("applied")?),
        )
        .order_by(applies::applied_at.desc().nulls_last())
        .order_by(applies::submitted_at.desc())
        .order_by(applies::id.desc())
        .first::<AppliedSchema>()
        .await?
        .map(|row| row.descriptor_sha256);
    if descriptor_sha256 == applied.as_deref() {
        Ok(())
    } else {
        Err(CatalogError::SchemaNotApplied {
            descriptor_sha256: descriptor_sha256.map(str::to_owned),
            applied_sha256: applied,
        })
    }
}

/// Lock an existing deployment row after the app row, matching the
/// collector's order, and refuse one whose retention has closed.
async fn existing_deployment(
    tx: &Database,
    app: &AppId,
    hash: &str,
) -> Result<Option<DeploymentId>, CatalogError> {
    let entity = tx.entity::<deploys::Entity>()?;
    let Some(found) = entity
        .query()
        .filter(
            deploys::app_id
                .eq(app.as_str())?
                .and(deploys::deploy_hash.eq(hash)?),
        )
        .first::<DeploymentRow>()
        .await?
    else {
        return Ok(None);
    };
    let row = deploys::app_id
        .eq(app.as_str())?
        .and(deploys::id.eq(found.id.as_str())?);
    changed(
        entity
            .update_many(row, deploys::retention_lock.increment(0_i64)?)
            .await?,
    )?;
    let current = entity
        .query()
        .filter(
            deploys::app_id
                .eq(app.as_str())?
                .and(deploys::id.eq(found.id.as_str())?),
        )
        .first::<DeploymentRow>()
        .await?
        .ok_or(CatalogError::Storage("locked deployment vanished"))?;
    match current.retention_state.as_str() {
        "available" => {}
        "reclaiming" | "deleted" => return Err(CatalogError::DeploymentReclaimed),
        _ => return Err(CatalogError::Storage("unknown deployment retention state")),
    }
    DeploymentId::parse(&current.id)
        .map(Some)
        .map_err(|_| CatalogError::Storage("deployment identity"))
}

/// Select the deployment for a verified manifest: reuse the app's row for the
/// same hash while it is still available, or record a new one.
async fn acquire_deployment(
    tx: &Database,
    app: &AppId,
    deployment: &VerifiedDeployment,
    now: UtcInstant,
) -> Result<DeploymentId, CatalogError> {
    if let Some(existing) = existing_deployment(tx, app, deployment.hash()).await? {
        changed(
            tx.entity::<deploys::Entity>()?
                .update_many(
                    deploys::app_id
                        .eq(app.as_str())?
                        .and(deploys::id.eq(existing.as_str())?),
                    deploys::manifest_json
                        .set(deployment.manifest_json())?
                        .and(deploys::activated_at.set(Some(now))?)?,
                )
                .await?,
        )?;
        return Ok(existing);
    }
    let id = DeploymentId::mint();
    let inserted = tx
        .entity::<deploys::Entity>()?
        .insert::<_, DeploymentRow>(NewDeployment {
            id: id.as_str(),
            app_id: app.as_str(),
            deploy_hash: deployment.hash(),
            manifest_json: deployment.manifest_json(),
            created_at: now,
            activated_at: Some(now),
        })
        .await?;
    if inserted.id != id.as_str() || inserted.retention_state != "available" {
        return Err(CatalogError::Storage("recorded deployment"));
    }
    Ok(id)
}

/// Allocate the next lifecycle revision under the app lock.
async fn allocate_revision(
    tx: &Database,
    app: &AppId,
    observed: i64,
) -> Result<Revision, CatalogError> {
    let next = observed
        .checked_add(1)
        .ok_or(CatalogError::RevisionExhausted)?;
    let revision =
        Revision::try_from(next).map_err(|_| CatalogError::Storage("lifecycle revision"))?;
    changed(
        tx.entity::<apps::Entity>()?
            .update_many(
                apps::id
                    .eq(app.as_str())?
                    .and(apps::lifecycle_revision.eq(observed)?),
                apps::lifecycle_revision.set(next)?,
            )
            .await?,
    )?;
    Ok(revision)
}

async fn insert_intent(
    tx: &Database,
    app: &AppId,
    revision: Revision,
    action: &'static str,
    activation: Option<(&DeploymentId, &str)>,
    now: UtcInstant,
) -> Result<(), CatalogError> {
    let id = zeroship_core::typed_id::new_lifecycle_intent_id();
    let inserted = tx
        .entity::<intents::Entity>()?
        .insert::<_, IntentId>(NewIntent {
            id: &id,
            app_id: app.as_str(),
            revision: revision.get(),
            action,
            deploy_id: activation.map(|(deployment, _)| deployment.as_str()),
            registration: activation.map(|(_, registration)| registration),
            state: PENDING,
            created_at: now,
        })
        .await?;
    if inserted.id == id {
        Ok(())
    } else {
        Err(CatalogError::Storage("lifecycle intent identity"))
    }
}

const fn changed(count: i64) -> Result<(), CatalogError> {
    if count == 1 {
        Ok(())
    } else {
        Err(CatalogError::Storage(
            "guarded catalog update did not apply",
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::{named, APPLICATION_NAME};

    #[test]
    fn catalog_sessions_offer_their_name_without_displacing_a_configured_one() {
        let offered = format!("fallback_application_name={APPLICATION_NAME}");
        for (url, expected) in [
            (
                "postgres://control@db:5432/zeroship",
                format!("postgres://control@db:5432/zeroship?{offered}"),
            ),
            (
                "postgresql://control@db/zeroship?sslmode=require",
                format!("postgresql://control@db/zeroship?sslmode=require&{offered}"),
            ),
            (
                "postgres://control@db/zeroship?",
                format!("postgres://control@db/zeroship?{offered}"),
            ),
        ] {
            assert_eq!(named(url), expected, "{url}");
        }
        for configured in [
            "postgres://control@db/zeroship?application_name=operator",
            "postgres://control@db/zeroship?fallback_application_name=operator",
            "postgres://control@db/zeroship?sslmode=require&application_name=operator",
            "not a url",
        ] {
            assert_eq!(named(configured), configured);
        }
    }
}
