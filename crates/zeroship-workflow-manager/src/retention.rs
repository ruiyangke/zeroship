//! Durable queue dependency intents over Control's deployment retention ledger.
#![expect(
    clippy::future_not_send,
    reason = "manager retention uses compio-local capabilities"
)]

use crate::{
    deployments::{self, DeploymentHolds},
    models::{
        jobs,
        schema::{deployment_holds as holds, schedule_activations, schedule_scopes, schedules},
        Job,
    },
    queue::{self, Budget},
    scheduling, Error, Queue,
};
use std::{fmt::Debug, future::Future, pin::Pin, time::Duration};
use zeroship_core::{
    app_id::AppId,
    typed_id,
    workflow_deployments::{HoldGeneration, HoldReceipt, HoldScope, HoldState},
    workflow_jobs::DeploymentId,
};
use zeroship_data_orm::orm::{Database, Filter, FromRow, Insertable};

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

enum Maintenance {
    Settled,
    Resume,
    Release(HoldGeneration),
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
    held_at: Option<i64>,
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

    /// One retention-lane turn for a hold. An unfinished intent resumes. A held
    /// deployment is released once the app's enabled calendar no longer selects
    /// it, its hold is older than `grace` and nothing depends on it. A hold in
    /// use stays held for a later turn; the policy never forces release.
    pub(crate) async fn maintain_deployment(
        &self,
        app: &AppId,
        deployment: &DeploymentId,
        grace: Duration,
    ) -> Result<(), Error> {
        let grace = i64::try_from(grace.as_millis()).map_err(|_| Error::Invalid)?;
        let budget = Budget::new(self.options.transaction_timeout);
        let scope = HoldScope::for_queue(app.clone());
        let step = self
            .transact_for(budget.clone(), |tx| {
                let scope = &scope;
                async move {
                    queue::lock_scope(&tx, app).await?;
                    let intent = read(&tx, app, deployment).await?.ok_or(Error::Conflict)?;
                    let generation = intent.validate(scope)?;
                    match intent.state.as_str() {
                        "acquiring" | "releasing" => return Ok(Maintenance::Resume),
                        "released" => return Ok(Maintenance::Settled),
                        "held" => {}
                        _ => return Err(Error::Storage),
                    }
                    // The candidate page may predate a reacquisition or a new
                    // selection, so both are decided again under the app lock.
                    let held_at = intent.held_at.ok_or(Error::Storage)?;
                    if held_at > self.clock.now().await?.saturating_sub(grace)
                        || scheduling::selected_deployment_in(&tx, app).await?.as_deref()
                            == Some(deployment.as_str())
                    {
                        return Ok(Maintenance::Settled);
                    }
                    match require_unused(&tx, app, deployment).await {
                        Ok(()) => {}
                        Err(Error::Conflict) => return Ok(Maintenance::Settled),
                        Err(error) => return Err(error),
                    }
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
                    Ok(Maintenance::Release(generation))
                }
            })
            .await?;
        let transition = match step {
            Maintenance::Settled => return Ok(()),
            Maintenance::Resume => None,
            Maintenance::Release(generation) => Some((generation, HoldState::Released)),
        };
        self.reconcile_for(app, deployment, transition, budget)
            .await
            .map(drop)
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
            // A confirmed hold restarts its release grace: the acquirer that
            // requested it commits its dependency within its own budget.
            let held_at = match desired {
                HoldState::Held => Some(self.clock.now().await?),
                HoldState::Released => {
                    require_unused(&tx, app, deployment).await?;
                    current.held_at
                }
            };
            if tx
                .entity::<holds::Entity>()?
                .update_many(
                    identity(app, deployment)?
                        .and(holds::generation.eq(generation.get())?)
                        .and(holds::state.eq(current.state.as_str())?),
                    holds::deploy_hash
                        .set(Some(receipt.deploy_hash.as_str()))?
                        .and(holds::state.set(desired.as_str())?)?
                        .and(holds::held_at.set(held_at)?)?,
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

/// The retention lane's candidates in hold identity order: unfinished intents,
/// and holds confirmed no later than `cutoff` that the app's enabled calendar
/// does not select and no unsettled job projects. Selection and use are joined
/// before the limit, so retained holds take no page slot; an unconfirmed hold
/// time is surfaced as damage. `Queue::maintain_deployment` decides each
/// candidate again under its app lock.
pub(crate) async fn maintenance_in<R: FromRow<holds::Entity>>(
    tx: &Database,
    cutoff: i64,
    after: Option<&str>,
    upper: Option<&str>,
    descending: bool,
    limit: u32,
) -> Result<Vec<R>, Error> {
    let hold = tx.entity::<holds::Entity>()?.alias("hold")?;
    let scope = tx.entity::<schedule_scopes::Entity>()?.alias("scope")?;
    let selected = tx.entity::<schedule_activations::Entity>()?.alias("selected")?;
    let job = tx.entity::<jobs::Entity>()?.alias("job")?;
    let mut filter = hold
        .column(holds::state)
        .eq("acquiring")?
        .or(hold.column(holds::state).eq("releasing")?)
        .or(hold
            .column(holds::state)
            .eq("held")?
            .and(
                hold.column(holds::held_at)
                    .is_null()
                    .or(hold.column(holds::held_at).lte(Some(cutoff))?),
            )
            .and(selected.column(schedule_activations::id).is_null())
            .and(job.column(jobs::id).is_null()));
    if let Some(after) = after {
        filter = filter.and(hold.column(holds::id).gt(after)?);
    }
    if let Some(upper) = upper {
        filter = filter.and(hold.column(holds::id).lte(upper)?);
    }
    Ok(tx
        .from(&hold)
        .left_join(
            &scope,
            scope
                .column(schedule_scopes::id)
                .eq(hold.column(holds::app_id))?
                .and(scope.column(schedule_scopes::enabled).eq(true)?),
        )?
        .left_join(
            &selected,
            selected
                .column(schedule_activations::app_id)
                .eq(scope.column(schedule_scopes::id))?
                .and(
                    selected
                        .column(schedule_activations::id)
                        .eq(scope.column(schedule_scopes::activation_id))?,
                )
                .and(
                    selected
                        .column(schedule_activations::deployment_id)
                        .eq(hold.column(holds::deployment_id))?,
                ),
        )?
        // Only a held row joins its jobs, so no candidate repeats per job.
        .left_join(
            &job,
            job.column(jobs::app_id)
                .eq(hold.column(holds::app_id))?
                .and(job.column(jobs::deployment_id).eq(hold.column(holds::deployment_id))?)
                .and(job.column(jobs::state).ne("settled")?)
                .and(hold.column(holds::state).eq("held")?),
        )?
        .filter(filter)
        .order_by(if descending {
            hold.column(holds::id).desc()
        } else {
            hold.column(holds::id).asc()
        })
        .select(hold.row::<R>())?
        .limit(i64::from(limit))?
        .all()
        .await?)
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
    // Validate the operation before trusting its nullable lookup projection.
    // A damaged projection must not hide an executable dependency from release.
    let mut after = None::<String>;
    loop {
        let mut filter = jobs::app_id
            .eq(app.as_str())?
            .and(jobs::state.ne("settled")?);
        if let Some(after) = &after {
            filter = filter.and(jobs::id.gt(after.as_str())?);
        }
        let page = tx
            .entity::<jobs::Entity>()?
            .query()
            .filter(filter)
            .order_by(jobs::id.asc())
            .limit(256)?
            .all::<Job>()
            .await?;
        if page.is_empty() {
            break;
        }
        for job in page {
            if !matches!(job.state.as_str(), "ready" | "leased") {
                return Err(Error::Storage);
            }
            if job.spec()?.deployment_id() == Some(deployment) {
                return Err(Error::Conflict);
            }
            after = Some(job.id);
        }
    }
    // Only an enabled calendar can publish from its frontiers. A disabled one
    // keeps them for restore, whose fresh activation holds the code again first.
    let schedule = tx.entity::<schedules::Entity>()?.alias("s")?;
    let activation = tx.entity::<schedule_activations::Entity>()?.alias("a")?;
    let calendar = tx.entity::<schedule_scopes::Entity>()?.alias("c")?;
    let live = tx
        .from(&schedule)
        .inner_join(
            &activation,
            schedule
                .column(schedules::app_id)
                .eq(activation.column(schedule_activations::app_id))?
                .and(
                    schedule
                        .column(schedules::activation_id)
                        .eq(activation.column(schedule_activations::id))?,
                ),
        )?
        .inner_join(
            &calendar,
            calendar
                .column(schedule_scopes::id)
                .eq(schedule.column(schedules::app_id))?
                .and(calendar.column(schedule_scopes::enabled).eq(true)?),
        )?
        .filter(
            schedule
                .column(schedules::app_id)
                .eq(app.as_str())?
                .and(
                    activation
                        .column(schedule_activations::deployment_id)
                        .eq(deployment.as_str())?,
                )
                .and(
                    schedule
                        .column(schedules::next_at)
                        .ne(None::<i64>)?
                        .or(schedule.column(schedules::catch_up_until).ne(None::<i64>)?)
                        .or(schedule
                            .column(schedules::catch_up_remaining)
                            .ne(None::<i64>)?),
                ),
        )
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
