//! Platform metadata retaining normal app deployments for customer workers.
//!
//! The host supplies an authorized platform database and holder scope. Customer
//! journals and payloads are never read here. Hold generations survive placement
//! expiry and fence retries from an earlier acquisition.

#![expect(
    clippy::future_not_send,
    reason = "ORM transactions stay on their compio thread"
)]

use crate::WorkflowServiceError;
use serde::{Deserialize, Serialize};
use zeroship_core::{app_id::AppId, typed_id};
use zeroship_data_orm::{
    error::DbError,
    orm::{Database, Entity, FindOptions, FromRow, Operation, Output},
    value, Value,
};

mod catalog;
mod local;

zeroship_data_orm::orm::schema!(pub models = "../../schema/deployments/schema.runtime.json");
use models::{app_deploy_holds as holds, app_deploys as deploys};

pub const RUNTIME_DESCRIPTOR: &str = include_str!("../../schema/deployments/schema.runtime.json");
pub const POSTGRES_SCHEMA: &str = include_str!("../../schema/deployments/postgres.sql");
pub const SQLITE_SCHEMA: &str = include_str!("../../schema/deployments/sqlite.sql");

/// Collection metadata for composition with the host's normal deployment models.
pub fn collections() -> Result<Vec<(String, Value)>, WorkflowServiceError> {
    let descriptor: Value =
        serde_json::from_str(RUNTIME_DESCRIPTOR).map_err(|_| invalid_storage())?;
    descriptor["collections"]
        .as_object()
        .ok_or_else(invalid_storage)?
        .iter()
        .map(|(name, collection)| {
            Ok((
                name.clone(),
                collection
                    .get("fields")
                    .ok_or_else(invalid_storage)?
                    .clone(),
            ))
        })
        .collect()
}

/// Stable host authority for a customer's journal, independent of worker instances.
#[derive(Clone, Debug)]
pub struct HoldScope {
    app: AppId,
    holder: String,
}
impl HoldScope {
    #[must_use]
    pub fn app(&self) -> &AppId {
        &self.app
    }

    #[must_use]
    pub fn holder(&self) -> &str {
        &self.holder
    }

    /// The host obtains these identities from its authenticated app assignment.
    pub fn new(app: AppId, holder: String) -> Result<Self, WorkflowServiceError> {
        typed_id::parse_with_prefix(&holder, "dhl").map_err(|_| {
            WorkflowServiceError::InvalidRequest("invalid deployment holder".into())
        })?;
        Ok(Self { app, holder })
    }
}

/// Host-authenticated access to deployment metadata for a single app journal.
/// A customer worker receives this client, never the platform database.
#[async_trait::async_trait(?Send)]
pub trait DeploymentHoldClient {
    fn scope(&self) -> &HoldScope;
    async fn acquire(
        &self,
        deployment: &str,
        generation: HoldGeneration,
    ) -> Result<HoldReceipt, WorkflowServiceError>;
    async fn release(
        &self,
        deployment: &str,
        generation: HoldGeneration,
    ) -> Result<HoldReceipt, WorkflowServiceError>;
}

/// Native metadata host binding. Remote customer workers use the same scoped
/// client contract over authenticated transport.
#[derive(Clone, Debug)]
pub struct ScopedDeploymentHolds {
    ledger: DeploymentHolds,
    scope: HoldScope,
}
#[async_trait::async_trait(?Send)]
impl DeploymentHoldClient for ScopedDeploymentHolds {
    fn scope(&self) -> &HoldScope {
        &self.scope
    }
    async fn acquire(
        &self,
        deployment: &str,
        generation: HoldGeneration,
    ) -> Result<HoldReceipt, WorkflowServiceError> {
        self.ledger
            .acquire(&self.scope, deployment, generation)
            .await
    }
    async fn release(
        &self,
        deployment: &str,
        generation: HoldGeneration,
    ) -> Result<HoldReceipt, WorkflowServiceError> {
        self.ledger
            .release(&self.scope, deployment, generation)
            .await
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "i64", into = "i64")]
pub struct HoldGeneration(i64);
impl HoldGeneration {
    #[must_use]
    pub const fn get(self) -> i64 {
        self.0
    }
    pub fn next(self) -> Result<Self, WorkflowServiceError> {
        self.0.checked_add(1).map(Self).ok_or_else(|| {
            WorkflowServiceError::ResourceExhausted("deployment hold generation exhausted".into())
        })
    }
}
impl TryFrom<i64> for HoldGeneration {
    type Error = &'static str;
    fn try_from(value: i64) -> Result<Self, Self::Error> {
        if value > 0 {
            Ok(Self(value))
        } else {
            Err("deployment hold generation must be positive")
        }
    }
}
impl From<HoldGeneration> for i64 {
    fn from(value: HoldGeneration) -> Self {
        value.0
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HoldState {
    Held,
    Released,
}
impl HoldState {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Held => "held",
            Self::Released => "released",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct HoldReceipt {
    pub app_id: AppId,
    pub deploy_id: String,
    pub holder_id: String,
    pub generation: HoldGeneration,
    pub state: HoldState,
    pub deploy_hash: String,
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
    #[must_use]
    pub fn for_scope(&self, scope: HoldScope) -> ScopedDeploymentHolds {
        ScopedDeploymentHolds {
            ledger: self.clone(),
            scope,
        }
    }
    pub fn new(database: Database) -> Result<Self, WorkflowServiceError> {
        database.entity::<deploys::Entity>()?;
        database.entity::<holds::Entity>()?;
        Ok(Self { database })
    }

    /// Acquire before the customer worker admits replay dependencies.
    /// Retrying the same held generation returns the same receipt.
    pub async fn acquire(
        &self,
        scope: &HoldScope,
        deployment: &str,
        generation: HoldGeneration,
    ) -> Result<HoldReceipt, WorkflowServiceError> {
        self.change(scope, deployment, generation, HoldState::Held)
            .await
    }

    /// Release only after the worker has durably closed admission and checked
    /// its own replay dependencies. Released rows remain as retry tombstones.
    pub async fn release(
        &self,
        scope: &HoldScope,
        deployment: &str,
        generation: HoldGeneration,
    ) -> Result<HoldReceipt, WorkflowServiceError> {
        self.change(scope, deployment, generation, HoldState::Released)
            .await
    }

    async fn change(
        &self,
        scope: &HoldScope,
        deployment: &str,
        generation: HoldGeneration,
        desired: HoldState,
    ) -> Result<HoldReceipt, WorkflowServiceError> {
        transact(&self.database, |tx| async move {
            let deploy = lock_deployment(&tx, &scope.app, deployment).await?;
            let app = scope.app.uuid().to_string();
            let filter =
                value!({"app_id":app, "deploy_id":deployment, "holder_id":scope.holder.clone()});
            let row = tx
                .entity::<holds::Entity>()?
                .find::<HoldRecord>(
                    holds::app_id
                        .eq(app.clone())?
                        .and(holds::deploy_id.eq(deployment.to_owned())?)
                        .and(holds::holder_id.eq(scope.holder.clone())?),
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
                            "app_id":app, "deploy_id":deployment, "holder_id":scope.holder.clone(),
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
            Ok(HoldReceipt {
                app_id: scope.app.clone(),
                deploy_id: deployment.into(),
                holder_id: scope.holder.clone(),
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
pub async fn fence_reclamation(
    tx: &Database,
    app: &AppId,
    deployment: &str,
) -> Result<String, WorkflowServiceError> {
    transact(tx, async |tx| {
        let record = lock_deployment(&tx, app, deployment).await?;
        // Unknown states and invalid generations cannot serve as release evidence.
        let retained = tx
            .collection(holds::Entity::COLLECTION)?
            .count(
                value!({"app_id":app.uuid().to_string(), "deploy_id":deployment,
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
                    value!({"app_id":app.uuid().to_string(), "id":deployment}),
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
pub async fn finish_reclamation(
    tx: &Database,
    app: &AppId,
    deployment: &str,
) -> Result<(), WorkflowServiceError> {
    transact(tx, async |tx| {
        let record = lock_deployment(&tx, app, deployment).await?;
        if record.retention_state == "available" {
            return Err(conflict("deployment reclamation was not fenced"));
        }
        tx.collection(deploys::Entity::COLLECTION)?
            .update(
                value!({"app_id":app.uuid().to_string(), "id":deployment}),
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
) -> Result<DeploymentRecord, WorkflowServiceError> {
    typed_id::parse_with_prefix(deployment, "dep")
        .map_err(|_| WorkflowServiceError::InvalidRequest("invalid deployment identity".into()))?;
    // A no-op ORM update takes the same writer lock on either backend, before
    // reading holds. Acquisition and reclamation serialize on this deployment.
    let changed = tx
        .collection(deploys::Entity::COLLECTION)?
        .execute(Operation::Update {
            filter: value!({"app_id":app.uuid().to_string(), "id":deployment}),
            patch: value!({"retention_lock":{"$inc":0}}),
            many: true,
        })
        .await?;
    if !matches!(changed, Output::Count(1)) {
        return Err(WorkflowServiceError::PermissionDenied);
    }
    let record = tx
        .entity::<deploys::Entity>()?
        .find::<DeploymentRecord>(
            deploys::app_id
                .eq(app.uuid().to_string())?
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
fn invalid_storage() -> WorkflowServiceError {
    WorkflowServiceError::Internal("invalid deployment retention metadata".into())
}
fn conflict(message: &str) -> WorkflowServiceError {
    WorkflowServiceError::Conflict(message.into())
}

/// Preserve workflow failures while asking the ORM to roll back their writes.
/// An ORM settlement failure takes precedence over the callback's own error.
async fn transact<T, F, Fut>(database: &Database, body: F) -> Result<T, WorkflowServiceError>
where
    F: FnOnce(Database) -> Fut,
    Fut: std::future::Future<Output = Result<T, WorkflowServiceError>>,
{
    const CALLBACK_FAILED: &str = "workflow_deployment_callback_failed";
    let mut callback_error = None;
    let saved = &mut callback_error;
    let result = database
        .transaction(|tx| async move {
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
