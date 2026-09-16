//! Platform metadata retaining normal app deployments for customer workers.
//!
//! The host supplies an authorized platform database and holder scope. Customer
//! journals and payloads are never read here. Hold generations survive placement
//! expiry and fence retries from an earlier acquisition.

#![expect(
    clippy::future_not_send,
    reason = "ORM transactions stay on their compio thread"
)]

pub use zeroship_core::workflow_deployments::{HoldGeneration, HoldReceipt, HoldScope, HoldState};
use zeroship_core::{app_id::AppId, typed_id};
use zeroship_data_orm::{
    error::DbError,
    orm::{Database, Entity, FindOptions, FromRow, Operation, Output},
    value,
};

#[cfg(test)]
use zeroship_data_orm::Value;

mod catalog;
mod error;
pub mod latest;
pub use error::Error;

mod schema_definition;
use models::{app_deploy_holds as holds, app_deploys as deploys};
pub use schema_definition::models;

pub const POSTGRES_SCHEMA: &str = include_str!("../../schema/deployments/postgres.sql");
pub const SQLITE_SCHEMA: &str = include_str!("../../schema/deployments/sqlite.sql");

/// Collection metadata for composition with the host's normal deployment models.
///
/// # Errors
/// Rejects invalid native model declarations.
pub fn collections() -> Result<zeroship_data_orm::schema::Schema, Error> {
    let schema = models::schema();
    schema.validate()?;
    Ok(schema)
}

#[derive(FromRow)]
#[orm(entity = deploys)]
struct DeploymentRecord {
    deploy_hash: String,
    retention_state: String,
}
#[derive(FromRow)]
#[orm(entity = holds)]
struct HoldRecord {
    generation: i64,
    state: String,
}

/// Hold operations on the platform's existing app deployment records.
#[derive(Clone, Debug)]
pub struct DeploymentHolds {
    database: Database,
}
impl DeploymentHolds {
    /// Bind the host's provisioned deployment catalog.
    ///
    /// # Errors
    /// Rejects missing collection metadata or metadata that differs from the models.
    pub fn new(database: Database) -> Result<Self, Error> {
        database.entity::<deploys::Entity>()?;
        database.entity::<holds::Entity>()?;
        Ok(Self { database })
    }

    /// Acquire before the customer worker admits replay dependencies.
    /// Retrying the same held generation returns the same receipt.
    ///
    /// # Errors
    /// Rejects foreign deployments, closed admission, stale hold generations,
    /// invalid retention metadata, and database failures.
    pub async fn acquire(
        &self,
        scope: &HoldScope,
        deployment: &str,
        generation: HoldGeneration,
    ) -> Result<HoldReceipt, Error> {
        self.change(scope, deployment, generation, HoldState::Held, || async {
            Ok(())
        })
        .await
    }

    /// Release only after the worker has durably closed admission and checked
    /// its own replay dependencies. Released rows remain as retry tombstones.
    ///
    /// # Errors
    /// Rejects foreign deployments, stale or unacquired hold generations,
    /// invalid retention metadata, and database failures.
    pub async fn release(
        &self,
        scope: &HoldScope,
        deployment: &str,
        generation: HoldGeneration,
    ) -> Result<HoldReceipt, Error> {
        self.change(
            scope,
            deployment,
            generation,
            HoldState::Released,
            || async { Ok(()) },
        )
        .await
    }

    /// Recheck remote placement after locking and before committing the hold.
    /// The host authenticates the caller before using this operation.
    ///
    /// # Errors
    /// Refuses stale holds, failed authority checks and catalog failures. Failed
    /// revalidation rolls back the hold mutation with the enclosing transaction.
    pub async fn acquire_authorized<F, Fut>(
        &self,
        scope: &HoldScope,
        deployment: &str,
        generation: HoldGeneration,
        authorize: F,
    ) -> Result<HoldReceipt, Error>
    where
        F: FnMut() -> Fut,
        Fut: std::future::Future<Output = Result<(), Error>>,
    {
        self.change(scope, deployment, generation, HoldState::Held, authorize)
            .await
    }

    /// Release with the same placement revalidation as acquisition.
    ///
    /// # Errors
    /// Refuses stale generations, failed authority checks and storage failures.
    pub async fn release_authorized<F, Fut>(
        &self,
        scope: &HoldScope,
        deployment: &str,
        generation: HoldGeneration,
        authorize: F,
    ) -> Result<HoldReceipt, Error>
    where
        F: FnMut() -> Fut,
        Fut: std::future::Future<Output = Result<(), Error>>,
    {
        self.change(
            scope,
            deployment,
            generation,
            HoldState::Released,
            authorize,
        )
        .await
    }

    async fn change<F, Fut>(
        &self,
        scope: &HoldScope,
        deployment: &str,
        generation: HoldGeneration,
        desired: HoldState,
        mut authorize: F,
    ) -> Result<HoldReceipt, Error>
    where
        F: FnMut() -> Fut,
        Fut: std::future::Future<Output = Result<(), Error>>,
    {
        transact(&self.database, |tx| async move {
            let deploy = lock_deployment(&tx, scope.app(), deployment).await?;
            authorize().await?;
            let app = scope.app().as_str();
            let filter =
                value!({"app_id":app, "deploy_id":deployment, "holder_id":scope.holder().to_owned()});
            let row = tx
                .entity::<holds::Entity>()?
                .find::<HoldRecord>(
                    holds::app_id
                        .eq(app)?
                        .and(holds::deploy_id.eq(deployment.to_owned())?)
                        .and(holds::holder_id.eq(scope.holder().to_owned())?),
                    FindOptions {
                        limit: Some(1),
                        ..Default::default()
                    },
                )
                .await?
                .into_iter()
                .next();
            if desired == HoldState::Held && deploy.retention_state != "available" {
                return Err(conflict("deployment reclamation has closed hold admission"));
            }
            if row.as_ref().is_some_and(|row| {
                row.generation <= 0 || !matches!(row.state.as_str(), "held" | "released")
            }) {
                return Err(invalid_storage());
            }
            match row {
                None if desired == HoldState::Held && generation.get() == 1 => {
                    tx.collection(holds::Entity::COLLECTION)?
                        .insert(value!({
                            "id":typed_id::generate("dhr"),
                            "app_id":app, "deploy_id":deployment, "holder_id":scope.holder().to_owned(),
                            "generation":generation.get(), "state":"held"
                        }))
                        .await?;
                }
                Some(row)
                    if row.generation == generation.get() && row.state == desired.as_str() => {}
                Some(row)
                    if (desired == HoldState::Released
                        && row.state == "held"
                        && row.generation == generation.get())
                        || (desired == HoldState::Held
                            && row.state == "released"
                            && row.generation.checked_add(1) == Some(generation.get())) =>
                {
                    tx.collection(holds::Entity::COLLECTION)?
                        .update(
                            filter,
                            value!({"generation":generation.get(), "state":desired.as_str()}),
                        )
                        .await?;
                }
                _ => {
                    return Err(conflict(
                        "deployment hold generation or transition is stale",
                    ))
                }
            }
            authorize().await?;
            Ok(HoldReceipt {
                app_id: scope.app().clone(),
                deploy_id: deployment.into(),
                holder_id: scope.holder().to_owned(),
                generation,
                state: desired,
                deploy_hash: deploy.deploy_hash,
            })
        })
        .await
    }
}

/// Close hold admission in the normal deployment collector's transaction.
///
/// The caller must first fence normal activation/routing and check its other
/// deployment consumers in this same transaction. Commit this fence before
/// deleting the manifest. A deletion retry can reuse the returned immutable hash.
/// Pass the database supplied by the host's `Database::transaction` callback
/// to compose with those checks. An ordinary database opens a transaction;
/// a callback database uses a savepoint inside its existing transaction.
///
/// # Errors
/// Rejects foreign deployments, missing release evidence, invalid retention
/// metadata, and database failures.
pub async fn fence_reclamation(
    tx: &Database,
    app: &AppId,
    deployment: &str,
) -> Result<String, Error> {
    transact(tx, async |tx| {
        let record = lock_deployment(&tx, app, deployment).await?;
        // Unknown states and invalid generations cannot serve as release evidence.
        let retained = tx
            .collection(holds::Entity::COLLECTION)?
            .count(
                value!({"app_id":app.as_str(), "deploy_id":deployment,
                    "$or":[{"state":{"$ne":"released"}}, {"generation":{"$lte":0}}]}),
                value!({}),
            )
            .await?;
        if !matches!(retained, Output::Count(0)) {
            return Err(conflict("deployment lacks valid release evidence"));
        }
        if record.retention_state == "available" {
            tx.collection(deploys::Entity::COLLECTION)?
                .update(
                    value!({"app_id":app.as_str(), "id":deployment}),
                    value!({"retention_state":"reclaiming"}),
                )
                .await?;
        }
        Ok(record.deploy_hash)
    })
    .await
}

/// Acknowledge manifest deletion without reopening admission or erasing tombstones.
/// Uses a transaction, or a savepoint when supplied a callback database.
///
/// # Errors
/// Rejects foreign deployments, reclamation that was not fenced, invalid
/// retention metadata, and database failures.
pub async fn finish_reclamation(tx: &Database, app: &AppId, deployment: &str) -> Result<(), Error> {
    transact(tx, async |tx| {
        let record = lock_deployment(&tx, app, deployment).await?;
        if record.retention_state == "available" {
            return Err(conflict("deployment reclamation was not fenced"));
        }
        tx.collection(deploys::Entity::COLLECTION)?
            .update(
                value!({"app_id":app.as_str(), "id":deployment}),
                value!({"retention_state":"deleted"}),
            )
            .await?;
        Ok(())
    })
    .await
}

async fn lock_deployment(
    tx: &Database,
    app: &AppId,
    deployment: &str,
) -> Result<DeploymentRecord, Error> {
    typed_id::parse_with_prefix(deployment, "dep")
        .map_err(|_| Error::InvalidRequest("invalid deployment identity".into()))?;
    // A no-op ORM update takes the same writer lock on either backend, before
    // reading holds. Acquisition and reclamation serialize on this deployment.
    let changed = tx
        .collection(deploys::Entity::COLLECTION)?
        .execute(Operation::Update {
            filter: value!({"app_id":app.as_str(), "id":deployment}),
            patch: value!({"retention_lock":{"$inc":0}}),
            many: true,
        })
        .await?;
    if !matches!(changed, Output::Count(1)) {
        return Err(Error::PermissionDenied);
    }
    let record = tx
        .entity::<deploys::Entity>()?
        .find::<DeploymentRecord>(
            deploys::app_id
                .eq(app.as_str())?
                .and(deploys::id.eq(deployment.to_owned())?),
            FindOptions {
                limit: Some(1),
                ..Default::default()
            },
        )
        .await?
        .into_iter()
        .next()
        .ok_or_else(invalid_storage)?;
    if !zeroship_bundle::validate_hash_format(&record.deploy_hash)
        || !matches!(
            record.retention_state.as_str(),
            "available" | "reclaiming" | "deleted"
        )
    {
        return Err(invalid_storage());
    }
    Ok(record)
}
fn invalid_storage() -> Error {
    Error::Internal("invalid deployment retention metadata".into())
}
fn conflict(message: &str) -> Error {
    Error::Conflict(message.into())
}

/// Preserve retention failures while asking the ORM to roll back their writes.
/// An ORM settlement failure takes precedence over the callback's own error.
async fn transact<T, F, Fut>(database: &Database, body: F) -> Result<T, Error>
where
    F: FnOnce(Database) -> Fut,
    Fut: std::future::Future<Output = Result<T, Error>>,
{
    const CALLBACK_FAILED: &str = "deployment_callback_failed";
    let mut callback_error = None;
    let saved = &mut callback_error;
    let result = database
        .transaction(|tx| {
            // Erase the callback's layout before nesting ORM transactions and
            // collector savepoints; cancellation still drops the owned future.
            let callback: std::pin::Pin<
                Box<dyn std::future::Future<Output = Result<T, DbError>> + '_>,
            > = Box::pin(async move {
                match body(tx).await {
                    Ok(value) => Ok(value),
                    Err(error) => {
                        *saved = Some(error);
                        Err(DbError::validation(
                            CALLBACK_FAILED,
                            "deployment transaction refused",
                        ))
                    }
                }
            });
            callback
        })
        .await;
    match result {
        Err(DbError::ValidationFailed {
            code: CALLBACK_FAILED,
            ..
        }) => Err(callback_error.unwrap_or_else(invalid_storage)),
        Err(error) => Err(error.into()),
        Ok(value) => Ok(value),
    }
}

#[cfg(test)]
mod tests;
