//! Customer-journal intents for the platform's deployment retention protocol.

#![expect(
    clippy::future_not_send,
    reason = "journal and hold clients use their host's compio thread"
)]

use super::{app::lock_app, models, store::Transaction, WorkflowService};
use crate::{
    deployment_holds::{DeploymentHoldClient, HoldGeneration, HoldReceipt, HoldScope, HoldState},
    WorkflowServiceError,
};
use zeroship_core::{app_id::AppId, typed_id};
use zeroship_data_orm::{
    orm::{Entity, FindOptions, FromRow, Operation, Output},
    sql::Predicate,
    value,
};

use models::deployment_holds as holds;

const MAX_PENDING_BATCH: u32 = 256;

#[derive(FromRow)]
#[orm(entity = holds)]
struct Intent {
    deploy_id: String,
    deploy_hash: String,
    holder_id: String,
    generation: i64,
    state: String,
}
impl Intent {
    fn validate(&self, scope: &HoldScope) -> Result<HoldGeneration, WorkflowServiceError> {
        if self.holder_id != scope.holder() {
            return Err(WorkflowServiceError::PermissionDenied);
        }
        if !zeroship_bundle::validate_hash_format(&self.deploy_hash)
            || !matches!(
                self.state.as_str(),
                "acquiring" | "held" | "releasing" | "released"
            )
        {
            return Err(invalid_storage());
        }
        self.generation.try_into().map_err(|_| invalid_storage())
    }

    fn receipt(
        &self,
        scope: &HoldScope,
        state: HoldState,
    ) -> Result<HoldReceipt, WorkflowServiceError> {
        Ok(HoldReceipt {
            app_id: scope.app().clone(),
            deploy_id: self.deploy_id.clone(),
            holder_id: self.holder_id.clone(),
            generation: self.validate(scope)?,
            deploy_hash: self.deploy_hash.clone(),
            state,
        })
    }
}

#[derive(FromRow)]
#[orm(entity = models::deploys)]
struct Deployment {
    hash: String,
    active: i64,
    state: String,
}

impl WorkflowService {
    /// Durably request a hold before admitting work from this app deployment.
    /// A failed or cancelled platform call leaves an acquisition to reconcile.
    pub async fn acquire_deployment_hold(
        &self,
        app: &AppId,
        deployment: &str,
        hash: &str,
        client: &dyn DeploymentHoldClient,
    ) -> Result<HoldReceipt, WorkflowServiceError> {
        validate_scope(app, deployment, client.scope())?;
        if !zeroship_bundle::validate_hash_format(hash) {
            return Err(WorkflowServiceError::InvalidRequest(
                "invalid deployment hash".into(),
            ));
        }
        let mut tx = self.begin().await?;
        lock_app(&mut tx, app).await?;
        let collection = tx.database().collection(holds::Entity::COLLECTION)?;
        match read_intent(&tx, app, deployment).await? {
            None => {
                collection
                    .insert(value!({
                        "app_id":app.as_str(), "deploy_id":deployment, "deploy_hash":hash,
                        "holder_id":client.scope().holder(), "generation":1, "state":"acquiring"
                    }))
                    .await?;
            }
            Some(intent) => {
                let generation = intent.validate(client.scope())?;
                if intent.deploy_hash != hash {
                    return Err(conflict("deployment hold identity is immutable"));
                }
                match intent.state.as_str() {
                    "acquiring" => {}
                    "held" => {
                        let receipt = intent.receipt(client.scope(), HoldState::Held)?;
                        tx.commit().await?;
                        return Ok(receipt);
                    }
                    "released" => {
                        collection.update(
                            identity(app, deployment),
                            value!({"generation":generation.next()?.get(), "state":"acquiring"}),
                        ).await?;
                    }
                    _ => {
                        return Err(conflict(
                            "deployment release must settle before reacquisition",
                        ))
                    }
                }
            }
        }
        tx.commit().await?;
        self.reconcile_deployment_hold(app, deployment, client)
            .await
    }

    /// Close admission and record release only when the customer journal has no
    /// retained execution or scheduling references. Terminal history still pins code.
    pub async fn release_deployment_hold(
        &self,
        app: &AppId,
        deployment: &str,
        client: &dyn DeploymentHoldClient,
    ) -> Result<HoldReceipt, WorkflowServiceError> {
        validate_scope(app, deployment, client.scope())?;
        let mut tx = self.begin().await?;
        lock_app(&mut tx, app).await?;
        let intent = read_intent(&tx, app, deployment)
            .await?
            .ok_or_else(missing)?;
        intent.validate(client.scope())?;
        match intent.state.as_str() {
            "held" => {
                close_admission(&tx, app, deployment, &intent.deploy_hash).await?;
                tx.database()
                    .collection(holds::Entity::COLLECTION)?
                    .update(identity(app, deployment), value!({"state":"releasing"}))
                    .await?;
            }
            "releasing" => {}
            "released" => {
                let receipt = intent.receipt(client.scope(), HoldState::Released)?;
                tx.commit().await?;
                return Ok(receipt);
            }
            _ => {
                return Err(conflict(
                    "deployment acquisition must settle before release",
                ))
            }
        }
        tx.commit().await?;
        self.reconcile_deployment_hold(app, deployment, client)
            .await
    }

    /// Retry durable acquisition or release after disconnection, lost replies or
    /// host restart. Platform I/O never holds a customer journal transaction open.
    pub async fn reconcile_deployment_hold(
        &self,
        app: &AppId,
        deployment: &str,
        client: &dyn DeploymentHoldClient,
    ) -> Result<HoldReceipt, WorkflowServiceError> {
        validate_scope(app, deployment, client.scope())?;
        let mut tx = self.begin().await?;
        lock_app(&mut tx, app).await?;
        let intent = read_intent(&tx, app, deployment)
            .await?
            .ok_or_else(missing)?;
        let generation = intent.validate(client.scope())?;
        tx.commit().await?;
        let receipt = match intent.state.as_str() {
            "acquiring" => client.acquire(deployment, generation).await?,
            "releasing" => client.release(deployment, generation).await?,
            "held" => return intent.receipt(client.scope(), HoldState::Held),
            "released" => return intent.receipt(client.scope(), HoldState::Released),
            _ => return Err(invalid_storage()),
        };
        let desired = if intent.state == "acquiring" {
            HoldState::Held
        } else {
            HoldState::Released
        };
        if receipt != intent.receipt(client.scope(), desired)? {
            return Err(conflict(
                "deployment hold acknowledgement does not match its intent",
            ));
        }
        let mut tx = self.begin().await?;
        lock_app(&mut tx, app).await?;
        let current = read_intent(&tx, app, deployment)
            .await?
            .ok_or_else(missing)?;
        current.validate(client.scope())?;
        let desired_state = if desired == HoldState::Held {
            "held"
        } else {
            "released"
        };
        if current.deploy_hash != intent.deploy_hash
            || current.generation != intent.generation
            || (current.state != intent.state && current.state != desired_state)
        {
            return Err(conflict("deployment hold acknowledgement is stale"));
        }
        tx.database()
            .collection(holds::Entity::COLLECTION)?
            .update(identity(app, deployment), value!({"state":desired_state}))
            .await?;
        tx.commit().await?;
        Ok(receipt)
    }

    /// Page pending intents for a host-authorized app before contacting its client.
    pub async fn pending_deployment_holds(
        &self,
        app: &AppId,
        after: Option<&str>,
        limit: u32,
    ) -> Result<Vec<String>, WorkflowServiceError> {
        if limit == 0 || limit > MAX_PENDING_BATCH {
            return Err(WorkflowServiceError::InvalidRequest(
                "invalid deployment hold page size".into(),
            ));
        }
        let mut tx = self.begin().await?;
        lock_app(&mut tx, app).await?;
        let db = tx.database();
        let source = db.entity::<holds::Entity>()?.alias("h")?;
        let mut predicates = vec![
            source.column(holds::app_id).eq(app.as_str())?,
            Predicate::Or(vec![
                source.column(holds::state).eq("acquiring")?,
                source.column(holds::state).eq("releasing")?,
            ]),
        ];
        if let Some(after) = after {
            typed_id::parse_with_prefix(after, "dep").map_err(|_| invalid_request())?;
            predicates.push(source.column(holds::deploy_id).gt(after)?);
        }
        let rows = db
            .from(&source)
            .filter(Predicate::And(predicates))
            .order_by(source.column(holds::deploy_id).asc())
            .select(source.row::<Intent>())?
            .limit(i64::from(limit))?
            .all()
            .await?;
        tx.commit().await?;
        Ok(rows.into_iter().map(|row| row.deploy_id).collect())
    }
}

async fn close_admission(
    tx: &Transaction,
    app: &AppId,
    deployment: &str,
    hash: &str,
) -> Result<(), WorkflowServiceError> {
    let records = tx
        .database()
        .entity::<models::deploys::Entity>()?
        .find::<Deployment>(
            models::deploys::app_id
                .eq(app.as_str())?
                .and(models::deploys::id.eq(deployment)?),
            FindOptions {
                limit: Some(1),
                ..Default::default()
            },
        )
        .await?;
    if let Some(record) = records.first() {
        if record.hash != hash
            || record.active != 0
            || !matches!(
                record.state.as_str(),
                "available" | "unavailable" | "retiring"
            )
        {
            return Err(conflict(
                "deployment is active or its journal identity is inconsistent",
            ));
        }
        tx.database()
            .collection(models::deploys::Entity::COLLECTION)?
            .update(
                value!({"app_id":app.as_str(), "id":deployment}),
                value!({"state":"retiring"}),
            )
            .await?;
    }
    for collection in [
        models::runs::Entity::COLLECTION,
        models::generations::Entity::COLLECTION,
        models::schedules::Entity::COLLECTION,
    ] {
        let retained = tx
            .database()
            .collection(collection)?
            .execute(Operation::Count {
                filter: value!({"app_id":app.as_str(), "deploy_id":deployment}),
                options: value!({}),
            })
            .await?;
        if !matches!(retained, Output::Count(0)) {
            return Err(conflict("deployment retains workflow journal dependencies"));
        }
    }
    Ok(())
}

async fn read_intent(
    tx: &Transaction,
    app: &AppId,
    deployment: &str,
) -> Result<Option<Intent>, WorkflowServiceError> {
    Ok(tx
        .database()
        .entity::<holds::Entity>()?
        .find::<Intent>(
            holds::app_id
                .eq(app.as_str())?
                .and(holds::deploy_id.eq(deployment)?),
            FindOptions {
                limit: Some(1),
                ..Default::default()
            },
        )
        .await?
        .into_iter()
        .next())
}

/// Retirement also fences an executable that has not yet been registered.
/// The loader retains this generation across I/O to reject an old preparation.
pub(super) async fn admission_generation(
    tx: &Transaction,
    app: &AppId,
    deployment: &str,
    hash: &str,
    scope: &HoldScope,
) -> Result<i64, WorkflowServiceError> {
    validate_scope(app, deployment, scope)?;
    let intent = read_intent(tx, app, deployment)
        .await?
        .ok_or_else(|| conflict("deployment hold is missing"))?;
    intent.validate(scope)?;
    if intent.state != "held" || intent.generation <= 0 || intent.deploy_hash != hash {
        return Err(conflict("deployment hold has not opened admission"));
    }
    Ok(intent.generation)
}
fn identity(app: &AppId, deployment: &str) -> zeroship_data_orm::Value {
    value!({"app_id":app.as_str(), "deploy_id":deployment})
}
fn validate_scope(
    app: &AppId,
    deployment: &str,
    scope: &HoldScope,
) -> Result<(), WorkflowServiceError> {
    if app != scope.app() {
        return Err(WorkflowServiceError::PermissionDenied);
    }
    typed_id::parse_with_prefix(deployment, "dep").map_err(|_| invalid_request())?;
    Ok(())
}
fn invalid_request() -> WorkflowServiceError {
    WorkflowServiceError::InvalidRequest("invalid deployment identity".into())
}
fn invalid_storage() -> WorkflowServiceError {
    WorkflowServiceError::Internal("invalid deployment hold journal".into())
}
fn missing() -> WorkflowServiceError {
    WorkflowServiceError::NotFound("deployment hold".into())
}
fn conflict(message: &str) -> WorkflowServiceError {
    WorkflowServiceError::Conflict(message.into())
}
