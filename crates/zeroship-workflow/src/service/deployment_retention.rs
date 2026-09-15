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
    value,
};

use models::deployment_holds as holds;

const MAX_PENDING_BATCH: u32 = 256;

#[derive(FromRow)]
#[orm(entity = holds)]
struct Intent {
    deploy_id: String,
    deploy_hash: Option<String>,
    holder_id: String,
    generation: i64,
    state: String,
}

#[derive(FromRow)]
#[orm(entity = holds)]
struct PendingHold {
    deploy_id: String,
}
impl Intent {
    fn validate(&self, scope: &HoldScope) -> Result<HoldGeneration, WorkflowServiceError> {
        if self.holder_id != scope.holder() {
            return Err(WorkflowServiceError::PermissionDenied);
        }
        let valid_hash = self.deploy_hash.as_deref().map_or_else(
            || self.state == "acquiring",
            zeroship_bundle::validate_hash_format,
        );
        if !valid_hash
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
            deploy_hash: self.hash()?.into(),
            state,
        })
    }

    fn hash(&self) -> Result<&str, WorkflowServiceError> {
        self.deploy_hash.as_deref().ok_or_else(invalid_storage)
    }

    fn matches_expected_hash(&self, expected: Option<&str>) -> Result<(), WorkflowServiceError> {
        if let (Some(stored), Some(expected)) = (self.deploy_hash.as_deref(), expected) {
            if stored != expected {
                return Err(conflict("deployment hold identity is immutable"));
            }
        }
        Ok(())
    }

    fn validate_receipt(
        &self,
        scope: &HoldScope,
        desired: HoldState,
        expected_hash: Option<&str>,
        receipt: &HoldReceipt,
    ) -> Result<(), WorkflowServiceError> {
        if receipt.app_id != *scope.app()
            || receipt.deploy_id != self.deploy_id
            || receipt.holder_id != self.holder_id
            || receipt.generation != self.validate(scope)?
            || receipt.state != desired
            || !zeroship_bundle::validate_hash_format(&receipt.deploy_hash)
            || self
                .deploy_hash
                .as_deref()
                .is_some_and(|hash| hash != receipt.deploy_hash)
            || expected_hash.is_some_and(|hash| hash != receipt.deploy_hash)
        {
            return Err(conflict(
                "deployment hold acknowledgement does not match its intent",
            ));
        }
        Ok(())
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
    ///
    /// # Errors
    /// Refuses foreign scopes, invalid identities, conflicting transitions or
    /// receipts, and unavailable journal or platform operations.
    pub async fn acquire_deployment_hold(
        &self,
        app: &AppId,
        deployment: &str,
        hash: &str,
        client: &dyn DeploymentHoldClient,
    ) -> Result<HoldReceipt, WorkflowServiceError> {
        self.acquire_deployment_hold_checked(app, deployment, Some(hash), client, &|| Ok(()))
            .await
    }

    /// A delivered job may resolve a deployment's immutable hash from the hold
    /// acknowledgement. Its authority must survive every journal commit and the
    /// external call; losing authority leaves the durable acquisition pending.
    pub(super) async fn acquire_deployment_hold_checked(
        &self,
        app: &AppId,
        deployment: &str,
        hash: Option<&str>,
        client: &dyn DeploymentHoldClient,
        check: &impl Fn() -> Result<(), WorkflowServiceError>,
    ) -> Result<HoldReceipt, WorkflowServiceError> {
        validate_scope(app, deployment, client.scope())?;
        if hash.is_some_and(|hash| !zeroship_bundle::validate_hash_format(hash)) {
            return Err(WorkflowServiceError::InvalidRequest(
                "invalid deployment hash".into(),
            ));
        }
        check()?;
        let mut tx = self.begin().await?;
        lock_app(&mut tx, app).await?;
        check()?;
        let collection = tx.database().collection(holds::Entity::COLLECTION)?;
        let generation = match read_intent(&tx, app, deployment).await? {
            None => {
                let generation = HoldGeneration::try_from(1).map_err(|_| invalid_storage())?;
                collection
                    .insert(value!({
                        "id":super::types::storage_id(), "app_id":app.as_str(), "deploy_id":deployment, "deploy_hash":hash,
                        "holder_id":client.scope().holder(), "generation":generation.get(), "state":"acquiring"
                    }))
                    .await?;
                generation
            }
            Some(intent) => {
                let generation = intent.validate(client.scope())?;
                intent.matches_expected_hash(hash)?;
                match intent.state.as_str() {
                    "acquiring" => generation,
                    "held" => {
                        let receipt = intent.receipt(client.scope(), HoldState::Held)?;
                        check()?;
                        tx.commit().await?;
                        return Ok(receipt);
                    }
                    "released" => {
                        let generation = generation.next()?;
                        collection
                            .update(
                                identity(app, deployment),
                                value!({"generation":generation.get(), "state":"acquiring"}),
                            )
                            .await?;
                        generation
                    }
                    _ => {
                        return Err(conflict(
                            "deployment release must settle before reacquisition",
                        ))
                    }
                }
            }
        };
        check()?;
        tx.commit().await?;
        self.reconcile_deployment_hold_checked(
            app,
            deployment,
            hash,
            Some((generation, HoldState::Held)),
            client,
            check,
        )
        .await
    }

    /// Close admission and record release only when the customer journal has no
    /// retained execution or scheduling references. Terminal history still pins code.
    ///
    /// # Errors
    /// Refuses foreign scopes, retained dependencies, inconsistent hold state,
    /// and failed journal or platform operations.
    pub async fn release_deployment_hold(
        &self,
        app: &AppId,
        deployment: &str,
        client: &dyn DeploymentHoldClient,
    ) -> Result<HoldReceipt, WorkflowServiceError> {
        self.release_deployment_hold_checked(app, deployment, client, &|| Ok(()))
            .await
    }

    /// A delivered release job holds this authority for the whole operation.
    /// Losing it leaves the durable intent where it stands: closed admission
    /// waits for its acknowledgement, and nothing is released without one.
    pub(super) async fn release_deployment_hold_checked(
        &self,
        app: &AppId,
        deployment: &str,
        client: &dyn DeploymentHoldClient,
        check: &impl Fn() -> Result<(), WorkflowServiceError>,
    ) -> Result<HoldReceipt, WorkflowServiceError> {
        validate_scope(app, deployment, client.scope())?;
        check()?;
        let mut tx = self.begin().await?;
        lock_app(&mut tx, app).await?;
        check()?;
        let intent = read_intent(&tx, app, deployment)
            .await?
            .ok_or_else(missing)?;
        let generation = intent.validate(client.scope())?;
        match intent.state.as_str() {
            "held" => {
                close_admission(&tx, app, deployment, intent.hash()?).await?;
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
        check()?;
        tx.commit().await?;
        self.reconcile_deployment_hold_checked(
            app,
            deployment,
            None,
            Some((generation, HoldState::Released)),
            client,
            check,
        )
        .await
    }

    /// Retry durable acquisition or release after disconnection, lost replies or
    /// host restart. Platform I/O never holds a customer journal transaction open.
    ///
    /// # Errors
    /// Refuses foreign scopes, invalid intents, stale acknowledgements, and
    /// unavailable journal or platform operations.
    pub async fn reconcile_deployment_hold(
        &self,
        app: &AppId,
        deployment: &str,
        client: &dyn DeploymentHoldClient,
    ) -> Result<HoldReceipt, WorkflowServiceError> {
        self.reconcile_deployment_hold_checked(app, deployment, None, None, client, &|| Ok(()))
            .await
    }

    pub(super) async fn reconcile_deployment_hold_checked(
        &self,
        app: &AppId,
        deployment: &str,
        expected_hash: Option<&str>,
        transition: Option<(HoldGeneration, HoldState)>,
        client: &dyn DeploymentHoldClient,
        check: &impl Fn() -> Result<(), WorkflowServiceError>,
    ) -> Result<HoldReceipt, WorkflowServiceError> {
        validate_scope(app, deployment, client.scope())?;
        check()?;
        let mut tx = self.begin().await?;
        lock_app(&mut tx, app).await?;
        check()?;
        let intent = read_intent(&tx, app, deployment)
            .await?
            .ok_or_else(missing)?;
        let generation = intent.validate(client.scope())?;
        intent.matches_expected_hash(expected_hash)?;
        if let Some((expected_generation, desired)) = transition {
            let matching_state = match desired {
                HoldState::Held => matches!(intent.state.as_str(), "acquiring" | "held"),
                HoldState::Released => matches!(intent.state.as_str(), "releasing" | "released"),
            };
            if generation != expected_generation || !matching_state {
                return Err(conflict("deployment hold transition is stale"));
            }
        }
        check()?;
        tx.commit().await?;
        check()?;
        let result = match intent.state.as_str() {
            "acquiring" => client.acquire(deployment, generation).await,
            "releasing" => client.release(deployment, generation).await,
            "held" => return intent.receipt(client.scope(), HoldState::Held),
            "released" => return intent.receipt(client.scope(), HoldState::Released),
            _ => return Err(invalid_storage()),
        };
        check()?;
        let receipt = result?;
        let desired = if intent.state == "acquiring" {
            HoldState::Held
        } else {
            HoldState::Released
        };
        intent.validate_receipt(client.scope(), desired, expected_hash, &receipt)?;
        let mut tx = self.begin().await?;
        lock_app(&mut tx, app).await?;
        check()?;
        let current = read_intent(&tx, app, deployment)
            .await?
            .ok_or_else(missing)?;
        current.validate(client.scope())?;
        let desired_state = if desired == HoldState::Held {
            "held"
        } else {
            "released"
        };
        let resolved_concurrently = intent.deploy_hash.is_none()
            && current.deploy_hash.as_deref() == Some(receipt.deploy_hash.as_str())
            && current.state == desired_state;
        if (current.deploy_hash != intent.deploy_hash && !resolved_concurrently)
            || current.generation != intent.generation
            || (current.state != intent.state && current.state != desired_state)
        {
            return Err(conflict("deployment hold acknowledgement is stale"));
        }
        tx.database()
            .collection(holds::Entity::COLLECTION)?
            .update(
                identity(app, deployment),
                value!({"state":desired_state, "deploy_hash":receipt.deploy_hash.clone()}),
            )
            .await?;
        check()?;
        tx.commit().await?;
        Ok(receipt)
    }

    /// Page pending intents for a host-authorized app before contacting its client.
    /// The cursor is an opaque stored key, so discovery can pass a malformed
    /// intent whose identity will be refused during reconciliation.
    ///
    /// # Errors
    /// Refuses invalid page bounds, unavailable app policy, and failed journal reads.
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
        // Discovery is only a hint. Reconciliation locks and revalidates the
        // selected intent, so scanning need not lock execution's app row.
        self.policy_for(app)?;
        let tx = self.begin().await?;
        let db = tx.database();
        let source = db.entity::<holds::Entity>()?.alias("h")?;
        let mut predicate = source.column(holds::app_id).eq(app.as_str())?.and(
            source
                .column(holds::state)
                .in_values(["acquiring", "releasing"])?,
        );
        if let Some(after) = after {
            predicate = predicate.and(source.column(holds::deploy_id).gt(after)?);
        }
        let rows = db
            .from(&source)
            .filter(predicate)
            .order_by(source.column(holds::deploy_id).asc())
            .select(source.row::<PendingHold>())?
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
    if super::publication::retains_deployment(tx, app, deployment).await? {
        return Err(conflict("deployment retains unpublished workflow jobs"));
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
    if intent.state != "held"
        || intent.generation <= 0
        || intent.deploy_hash.as_deref() != Some(hash)
    {
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
