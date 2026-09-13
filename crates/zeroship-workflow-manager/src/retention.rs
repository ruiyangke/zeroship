//! Durable queue dependency intents over Control's deployment retention ledger.
#![expect(
    clippy::future_not_send,
    reason = "manager retention uses compio-local capabilities"
)]

use crate::{
    deployments::{self, DeploymentHolds},
    models::{
        jobs, recovery_scopes,
        schema::{deployment_holds as holds, schedule_activations, schedules},
    },
    queue::{self, Budget},
    Error, Queue,
};
use std::{fmt::Debug, future::Future, pin::Pin};
use zeroship_core::{
    app_id::AppId,
    typed_id,
    workflow_deployments::{HoldGeneration, HoldReceipt, HoldScope, HoldState},
    workflow_jobs::DeploymentId,
};
use zeroship_data_orm::{
    orm::{Database, Filter, FromRow, Insertable},
    sql::Predicate,
};

/// A host supplies a local catalog capability or authenticated Control transport.
pub type HoldFuture<'a> = Pin<Box<dyn Future<Output = Result<HoldReceipt, Error>> + 'a>>;

pub trait HoldClient: Debug {
    fn acquire<'a>(
        &'a self,
        app: &'a AppId,
        deployment: &'a DeploymentId,
        generation: HoldGeneration,
    ) -> HoldFuture<'a>;
    fn release<'a>(
        &'a self,
        app: &'a AppId,
        deployment: &'a DeploymentId,
        generation: HoldGeneration,
    ) -> HoldFuture<'a>;
}

pub(crate) enum Retention<T> {
    Ready(T),
    Acquire(DeploymentId),
}

#[derive(Insertable)]
#[orm(entity = holds)]
struct NewIntent<'a> {
    id: String,
    app_id: &'a str,
    deployment_id: &'a str,
    holder_id: &'a str,
    deploy_hash: Option<&'a str>,
    generation: i64,
    state: &'a str,
}

/// A fresh publication may prepare a hold; an accepted dependency must already
/// have one. Releasing state closes admission until its acknowledgement settles.
pub(crate) async fn prepared(
    tx: &Database,
    app: &AppId,
    deployment: &DeploymentId,
) -> Result<bool, Error> {
    let Some(intent) = read(tx, app, deployment).await? else {
        return Ok(false);
    };
    intent.validate(&HoldScope::for_queue(app.clone()))?;
    match intent.state.as_str() {
        "held" => Ok(true),
        "acquiring" | "released" => Ok(false),
        _ => Err(Error::Conflict),
    }
}

/// Native composition uses the same Control ledger without sharing its database
/// handle with queue operations. Replicas must share the authoritative app queue.
#[derive(Debug, Clone)]
pub struct CatalogClient(DeploymentHolds);
impl CatalogClient {
    #[must_use]
    pub const fn new(ledger: DeploymentHolds) -> Self {
        Self(ledger)
    }
}
impl HoldClient for CatalogClient {
    fn acquire<'a>(
        &'a self,
        app: &'a AppId,
        deployment: &'a DeploymentId,
        generation: HoldGeneration,
    ) -> HoldFuture<'a> {
        Box::pin(async move {
            self.0
                .acquire(
                    &HoldScope::for_queue(app.clone()),
                    deployment.as_str(),
                    generation,
                )
                .await
                .map_err(|error| catalog_error(&error))
        })
    }
    fn release<'a>(
        &'a self,
        app: &'a AppId,
        deployment: &'a DeploymentId,
        generation: HoldGeneration,
    ) -> HoldFuture<'a> {
        Box::pin(async move {
            self.0
                .release(
                    &HoldScope::for_queue(app.clone()),
                    deployment.as_str(),
                    generation,
                )
                .await
                .map_err(|error| catalog_error(&error))
        })
    }
}

#[derive(FromRow)]
#[orm(entity = holds)]
struct Intent {
    holder_id: String,
    deploy_hash: Option<String>,
    generation: i64,
    state: String,
}
impl Intent {
    fn validate(&self, scope: &HoldScope) -> Result<HoldGeneration, Error> {
        if self.holder_id != scope.holder()
            || !matches!(
                self.state.as_str(),
                "acquiring" | "held" | "releasing" | "released"
            )
            || !self.deploy_hash.as_deref().map_or_else(
                || self.state == "acquiring",
                zeroship_bundle::validate_hash_format,
            )
        {
            return Err(Error::Storage);
        }
        self.generation.try_into().map_err(|_| Error::Storage)
    }
    fn receipt(
        &self,
        scope: &HoldScope,
        deployment: &DeploymentId,
        state: HoldState,
    ) -> Result<HoldReceipt, Error> {
        Ok(HoldReceipt {
            app_id: scope.app().clone(),
            deploy_id: deployment.as_str().to_owned(),
            holder_id: scope.holder().to_owned(),
            generation: self.validate(scope)?,
            state,
            deploy_hash: self.deploy_hash.clone().ok_or(Error::Storage)?,
        })
    }
    fn check_receipt(
        &self,
        scope: &HoldScope,
        deployment: &DeploymentId,
        state: HoldState,
        receipt: &HoldReceipt,
    ) -> Result<(), Error> {
        if receipt.app_id != *scope.app()
            || receipt.deploy_id != deployment.as_str()
            || receipt.holder_id != scope.holder()
            || receipt.generation != self.validate(scope)?
            || receipt.state != state
            || !zeroship_bundle::validate_hash_format(&receipt.deploy_hash)
            || self
                .deploy_hash
                .as_deref()
                .is_some_and(|hash| hash != receipt.deploy_hash)
        {
            return Err(Error::Conflict);
        }
        Ok(())
    }
}

impl Queue {
    /// Acquire a durable queue hold before publishing executable dependencies.
    ///
    /// # Errors
    /// Refuses unregistered apps, conflicting generations and failed storage or transport.
    pub async fn ensure_deployment(
        &self,
        app: &AppId,
        deployment: &DeploymentId,
    ) -> Result<HoldReceipt, Error> {
        self.ensure_deployment_for(
            app,
            deployment,
            Budget::new(self.options.transaction_timeout),
        )
        .await
    }

    pub(crate) async fn ensure_deployment_for(
        &self,
        app: &AppId,
        deployment: &DeploymentId,
        budget: Budget,
    ) -> Result<HoldReceipt, Error> {
        let scope = HoldScope::for_queue(app.clone());
        let prepared = self
            .transact_for(budget.clone(), |tx| async move {
                queue::lock_scope(&tx, app).await?;
                let generation = if let Some(intent) = read(&tx, app, deployment).await? {
                    let generation = intent.validate(&scope)?;
                    match intent.state.as_str() {
                        "held" => {
                            return Ok(Ok(intent.receipt(&scope, deployment, HoldState::Held)?))
                        }
                        "acquiring" => generation,
                        "released" => {
                            let next = generation.next().map_err(|_| Error::Capacity)?;
                            if tx
                                .entity::<holds::Entity>()?
                                .update_many(
                                    identity(app, deployment)?
                                        .and(holds::generation.eq(generation.get())?)
                                        .and(holds::state.eq("released")?),
                                    holds::generation
                                        .set(next.get())?
                                        .and(holds::state.set("acquiring")?)?,
                                )
                                .await?
                                != 1
                            {
                                return Err(Error::Storage);
                            }
                            next
                        }
                        _ => return Err(Error::Conflict),
                    }
                } else {
                    let generation = HoldGeneration::try_from(1).map_err(|_| Error::Storage)?;
                    tx.entity::<holds::Entity>()?
                        .insert::<_, Intent>(NewIntent {
                            id: typed_id::generate("dhi"),
                            app_id: app.as_str(),
                            deployment_id: deployment.as_str(),
                            holder_id: scope.holder(),
                            deploy_hash: None,
                            generation: generation.get(),
                            state: "acquiring",
                        })
                        .await?;
                    generation
                };
                Ok(Err(generation))
            })
            .await?;
        match prepared {
            Ok(receipt) => Ok(receipt),
            Err(generation) => {
                self.reconcile_for(app, deployment, Some((generation, HoldState::Held)), budget)
                    .await
            }
        }
    }

    /// Close new publication under the app lock before releasing Control's hold.
    /// Historical receipts alone do not retain code.
    ///
    /// # Errors
    /// Refuses live queue dependencies, invalid transitions and failed storage or transport.
    pub async fn release_deployment(
        &self,
        app: &AppId,
        deployment: &DeploymentId,
    ) -> Result<HoldReceipt, Error> {
        let budget = Budget::new(self.options.transaction_timeout);
        let scope = HoldScope::for_queue(app.clone());
        let prepared = self
            .transact_for(budget.clone(), |tx| async move {
                queue::lock_scope(&tx, app).await?;
                let intent = read(&tx, app, deployment).await?.ok_or(Error::Conflict)?;
                let generation = intent.validate(&scope)?;
                match intent.state.as_str() {
                    "released" => {
                        return Ok(Ok(intent.receipt(
                            &scope,
                            deployment,
                            HoldState::Released,
                        )?))
                    }
                    "held" => {
                        require_unused(&tx, app, deployment).await?;
                        if tx
                            .entity::<holds::Entity>()?
                            .update_many(
                                identity(app, deployment)?
                                    .and(holds::generation.eq(generation.get())?)
                                    .and(holds::state.eq("held")?),
                                holds::state.set("releasing")?,
                            )
                            .await?
                            != 1
                        {
                            return Err(Error::Storage);
                        }
                    }
                    "releasing" => require_unused(&tx, app, deployment).await?,
                    _ => return Err(Error::Conflict),
                }
                Ok(Err(generation))
            })
            .await?;
        match prepared {
            Ok(receipt) => Ok(receipt),
            Err(generation) => {
                self.reconcile_for(
                    app,
                    deployment,
                    Some((generation, HoldState::Released)),
                    budget,
                )
                .await
            }
        }
    }

    /// Recover a durable intent after loss of the manager or its Control response.
    ///
    /// # Errors
    /// Refuses changed receipts, stale generations and failed storage or transport.
    pub async fn reconcile_deployment(
        &self,
        app: &AppId,
        deployment: &DeploymentId,
    ) -> Result<HoldReceipt, Error> {
        self.reconcile_for(
            app,
            deployment,
            None,
            Budget::new(self.options.transaction_timeout),
        )
        .await
    }

    async fn reconcile_for(
        &self,
        app: &AppId,
        deployment: &DeploymentId,
        transition: Option<(HoldGeneration, HoldState)>,
        budget: Budget,
    ) -> Result<HoldReceipt, Error> {
        let scope = HoldScope::for_queue(app.clone());
        let intent = self
            .transact_for(budget.clone(), |tx| {
                let scope = &scope;
                async move {
                    queue::lock_scope(&tx, app).await?;
                    let intent = read(&tx, app, deployment).await?.ok_or(Error::Conflict)?;
                    let generation = intent.validate(scope)?;
                    if let Some((expected, desired)) = transition {
                        if generation != expected || !matches_transition(&intent.state, desired) {
                            return Err(Error::Conflict);
                        }
                    }
                    if matches!(intent.state.as_str(), "releasing" | "released") {
                        require_unused(&tx, app, deployment).await?;
                    }
                    Ok(intent)
                }
            })
            .await?;
        let generation = intent.validate(&scope)?;
        let desired = match intent.state.as_str() {
            "held" => return intent.receipt(&scope, deployment, HoldState::Held),
            "released" => return intent.receipt(&scope, deployment, HoldState::Released),
            "acquiring" => HoldState::Held,
            "releasing" => HoldState::Released,
            _ => return Err(Error::Storage),
        };
        let receipt = queue::bounded(budget.clone(), async {
            match desired {
                HoldState::Held => self.holds.acquire(app, deployment, generation).await,
                HoldState::Released => self.holds.release(app, deployment, generation).await,
            }
        })
        .await??;
        intent.check_receipt(&scope, deployment, desired, &receipt)?;
        self.transact_for(budget, |tx| async move {
            queue::lock_scope(&tx, app).await?;
            let current = read(&tx, app, deployment).await?.ok_or(Error::Storage)?;
            current.validate(&scope)?;
            if current.generation != generation.get()
                || !matches_transition(&current.state, desired)
            {
                return Err(Error::Conflict);
            }
            current.check_receipt(&scope, deployment, desired, &receipt)?;
            if desired == HoldState::Released {
                require_unused(&tx, app, deployment).await?;
            }
            if tx
                .entity::<holds::Entity>()?
                .update_many(
                    identity(app, deployment)?
                        .and(holds::generation.eq(generation.get())?)
                        .and(holds::state.eq(current.state.as_str())?),
                    holds::deploy_hash
                        .set(Some(receipt.deploy_hash.as_str()))?
                        .and(holds::state.set(desired.as_str())?)?,
                )
                .await?
                != 1
            {
                return Err(Error::Storage);
            }
            Ok(receipt)
        })
        .await
    }

    /// List this app's durable intents for bounded host reconciliation and release.
    ///
    /// # Errors
    /// Refuses invalid page limits, malformed identities and failed storage.
    pub async fn deployment_intents(
        &self,
        app: &AppId,
        after: Option<&DeploymentId>,
        limit: u32,
    ) -> Result<Vec<DeploymentId>, Error> {
        if limit == 0 || i64::from(limit) > zeroship_data_orm::sql::MAX_ROW_LIMIT {
            return Err(Error::Invalid);
        }
        self.transact(|tx| async move {
            let mut filter = holds::app_id.eq(app.as_str())?;
            if let Some(after) = after {
                filter = filter.and(holds::deployment_id.gt(after.as_str())?);
            }
            tx.entity::<holds::Entity>()?
                .query()
                .filter(filter)
                .order_by(holds::deployment_id.asc())
                .limit(i64::from(limit))?
                .all::<Dependency>()
                .await?
                .into_iter()
                .map(|row| DeploymentId::parse(&row.deployment_id).map_err(|_| Error::Storage))
                .collect()
        })
        .await
    }
}

#[derive(FromRow)]
#[orm(entity = holds)]
struct Dependency {
    deployment_id: String,
}

pub(crate) async fn require_held(
    tx: &Database,
    app: &AppId,
    deployment: &DeploymentId,
) -> Result<HoldReceipt, Error> {
    let scope = HoldScope::for_queue(app.clone());
    let intent = read(tx, app, deployment).await?.ok_or(Error::Conflict)?;
    intent.validate(&scope)?;
    if intent.state != "held" {
        return Err(Error::Conflict);
    }
    intent.receipt(&scope, deployment, HoldState::Held)
}

async fn read(
    tx: &Database,
    app: &AppId,
    deployment: &DeploymentId,
) -> Result<Option<Intent>, Error> {
    Ok(tx
        .entity::<holds::Entity>()?
        .query()
        .filter(identity(app, deployment)?)
        .first::<Intent>()
        .await?)
}
fn identity(app: &AppId, deployment: &DeploymentId) -> Result<Filter<holds::Entity>, Error> {
    Ok(holds::app_id
        .eq(app.as_str())?
        .and(holds::deployment_id.eq(deployment.as_str())?))
}
fn matches_transition(state: &str, desired: HoldState) -> bool {
    match desired {
        HoldState::Held => matches!(state, "acquiring" | "held"),
        HoldState::Released => matches!(state, "releasing" | "released"),
    }
}

#[derive(FromRow)]
#[orm(entity = schedules)]
struct LiveSchedule {
    id: String,
}

async fn require_unused(
    tx: &Database,
    app: &AppId,
    deployment: &DeploymentId,
) -> Result<(), Error> {
    let pending = tx
        .entity::<jobs::Entity>()?
        .exists(
            jobs::app_id
                .eq(app.as_str())?
                .and(jobs::deployment_id.eq(deployment.as_str())?)
                .and(jobs::state.ne("settled")?),
        )
        .await?;
    let recovery = tx
        .entity::<recovery_scopes::Entity>()?
        .exists(
            recovery_scopes::id
                .eq(app.as_str())?
                .and(recovery_scopes::deployment_id.eq(deployment.as_str())?),
        )
        .await?;
    if pending || recovery {
        return Err(Error::Conflict);
    }
    let schedule = tx.entity::<schedules::Entity>()?.alias("s")?;
    let activation = tx.entity::<schedule_activations::Entity>()?.alias("a")?;
    let live = tx
        .from(&schedule)
        .inner_join(
            &activation,
            Predicate::And(vec![
                schedule
                    .column(schedules::app_id)
                    .eq_column(activation.column(schedule_activations::app_id))?,
                schedule
                    .column(schedules::activation_id)
                    .eq_column(activation.column(schedule_activations::id))?,
            ]),
        )?
        .filter(Predicate::And(vec![
            schedule.column(schedules::app_id).eq(app.as_str())?,
            activation
                .column(schedule_activations::deployment_id)
                .eq(deployment.as_str())?,
            Predicate::Or(vec![
                schedule.column(schedules::next_at).ne(None::<i64>)?,
                schedule.column(schedules::catch_up_until).ne(None::<i64>)?,
                schedule
                    .column(schedules::catch_up_remaining)
                    .ne(None::<i64>)?,
            ]),
        ]))
        .select(schedule.row::<LiveSchedule>())?
        .limit(1)?
        .all()
        .await?;
    if let Some(schedule) = live.first() {
        zeroship_core::workflow_schedules::ScheduleId::parse(&schedule.id)
            .map_err(|_| Error::Storage)?;
        return Err(Error::Conflict);
    }
    Ok(())
}

const fn catalog_error(error: &deployments::Error) -> Error {
    use deployments::Error as Source;
    match error {
        Source::InvalidRequest(_) => Error::Invalid,
        Source::Unauthenticated | Source::PermissionDenied => Error::Denied,
        Source::Conflict(_) => Error::Conflict,
        Source::ResourceExhausted(_) => Error::Capacity,
        Source::Unavailable(_) => Error::Unavailable,
        Source::Timeout => Error::Timeout,
        Source::Internal(_) => Error::Storage,
    }
}
