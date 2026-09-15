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
    workflow_jobs::{DeploymentId, JobId, JobOperation, JobOutcome, JobSpec},
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
    journal_state: &'a str,
}

/// The manager's view of the journal holder's duty for one deployment. It never
/// holds journal retention itself: a released queue hold only makes the journal
/// holder's dependency the last one, and the release job asks the creator engine
/// to check its own journal and give the deployment back.
const JOURNAL_PENDING: &str = "pending";
const JOURNAL_RELEASING: &str = "releasing";
const JOURNAL_RELEASED: &str = "released";

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
    journal_state: String,
    journal_job_id: Option<String>,
    journal_published_at: Option<i64>,
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
        self.validate_journal()?;
        self.generation.try_into().map_err(|_| Error::Storage)
    }

    /// A release job identity is recorded only while one is in flight, so a
    /// settled reply cannot be attributed to a duty that was reset or finished.
    fn validate_journal(&self) -> Result<(), Error> {
        if !matches!(
            self.journal_state.as_str(),
            JOURNAL_PENDING | JOURNAL_RELEASING | JOURNAL_RELEASED
        ) || (self.journal_state == JOURNAL_RELEASING) != self.journal_job_id.is_some()
            || self.journal_published_at.is_some_and(|at| at < 0)
        {
            return Err(Error::Storage);
        }
        if let Some(job) = &self.journal_job_id {
            JobId::parse(job).map_err(|_| Error::Storage)?;
        }
        Ok(())
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
                            // Reacquisition tombstones the journal duty: the
                            // creator engine will take a fresh journal hold for
                            // this deployment, so a release already in flight
                            // must not report the new one released.
                            if tx
                                .entity::<holds::Entity>()?
                                .update_many(
                                    identity(app, deployment)?
                                        .and(holds::generation.eq(generation.get())?)
                                        .and(holds::state.eq("released")?),
                                    holds::generation
                                        .set(next.get())?
                                        .and(holds::state.set("acquiring")?)?
                                        .and(holds::journal_state.set(JOURNAL_PENDING)?)?
                                        .and(holds::journal_job_id.set(None::<&str>)?)?
                                        .and(holds::journal_published_at.set(None::<i64>)?)?,
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
                            journal_state: JOURNAL_PENDING,
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
    ///
    /// A released queue hold leaves the journal holder. The same turn carries
    /// that deployment's journal release duty: it publishes one release job for
    /// the creator engine under the same conditions, and applies the settled
    /// reply of an earlier one. Neither holder class can release the other.
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
                        "released" => {
                            self.maintain_journal(&tx, app, deployment, &intent, grace)
                                .await?;
                            return Ok(Maintenance::Settled);
                        }
                        "held" => {}
                        _ => return Err(Error::Storage),
                    }
                    // The candidate page may predate a reacquisition or a new
                    // selection, so both are decided again under the app lock.
                    if !self
                        .unused_deployment(&tx, app, deployment, intent.held_at, grace)
                        .await?
                    {
                        return Ok(Maintenance::Settled);
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

    /// Whether this deployment is old enough and free enough to give back. The
    /// caller holds the app lock, so a reacquisition, a fresh selection or a new
    /// job published after the candidate page was read is seen here. A hold
    /// confirmed no later than `grace` ago is left alone: its acquirer confirmed
    /// it outside the queue transaction and commits its dependency within the
    /// transaction budget, which `Driver::new` requires the grace to exceed.
    /// A held row without a confirmation time is damage, not an idle deployment.
    async fn unused_deployment(
        &self,
        tx: &Database,
        app: &AppId,
        deployment: &DeploymentId,
        held_at: Option<i64>,
        grace: i64,
    ) -> Result<bool, Error> {
        let held_at = held_at.ok_or(Error::Storage)?;
        if held_at > self.clock.now().await?.saturating_sub(grace)
            || scheduling::selected_deployment_in(tx, app)
                .await?
                .as_deref()
                == Some(deployment.as_str())
        {
            return Ok(false);
        }
        match require_unused(tx, app, deployment).await {
            Ok(()) => Ok(true),
            Err(Error::Conflict) => Ok(false),
            Err(error) => Err(error),
        }
    }

    /// One journal release turn for a deployment whose queue hold is released.
    /// The caller holds the app lock. Workers cannot reach this: publication is
    /// the manager's, and the creator engine only answers the delivered job.
    async fn maintain_journal(
        &self,
        tx: &Database,
        app: &AppId,
        deployment: &DeploymentId,
        intent: &Intent,
        grace: i64,
    ) -> Result<(), Error> {
        match intent.journal_state.as_str() {
            JOURNAL_RELEASED => Ok(()),
            JOURNAL_RELEASING => {
                let pending = intent.journal_job_id.as_deref().ok_or(Error::Storage)?;
                let Some(settled) = self.settled_release(tx, app, deployment, pending).await?
                else {
                    return Ok(());
                };
                self.finish_journal(tx, app, deployment, pending, settled)
                    .await
            }
            JOURNAL_PENDING => {
                let now = self.clock.now().await?;
                if intent
                    .journal_published_at
                    .is_some_and(|at| at > now.saturating_sub(grace))
                    || !self
                        .unused_deployment(tx, app, deployment, intent.held_at, grace)
                        .await?
                {
                    return Ok(());
                }
                let spec = JobSpec {
                    id: JobId::mint(),
                    app_id: app.clone(),
                    operation: JobOperation::ReleaseHold {
                        deployment_id: deployment.clone(),
                    },
                    available_at: now.try_into().map_err(|_| Error::Storage)?,
                };
                self.insert(tx, &spec, now).await?;
                changed_once(
                    tx.entity::<holds::Entity>()?
                        .update_many(
                            identity(app, deployment)?
                                .and(holds::journal_state.eq(JOURNAL_PENDING)?)
                                .and(holds::journal_job_id.eq(None::<&str>)?),
                            holds::journal_state
                                .set(JOURNAL_RELEASING)?
                                .and(holds::journal_job_id.set(Some(spec.id.as_str()))?)?
                                .and(holds::journal_published_at.set(Some(now))?)?,
                        )
                        .await?,
                )
            }
            _ => Err(Error::Storage),
        }
    }

    /// The settled outcome of a published release, or `None` while it is live.
    /// A reply that does not name this app's release of this deployment is
    /// damage: the duty records the job it published and nothing else.
    async fn settled_release(
        &self,
        tx: &Database,
        app: &AppId,
        deployment: &DeploymentId,
        pending: &str,
    ) -> Result<Option<JobOutcome>, Error> {
        let job = queue::load(tx, app, pending).await?.ok_or(Error::Storage)?;
        let spec = job.spec()?;
        if spec.app_id != *app
            || spec.id.as_str() != pending
            || spec.released_deployment() != Some(deployment)
        {
            return Err(Error::Storage);
        }
        match job.state.as_str() {
            "ready" | "leased" => Ok(None),
            "settled" => {
                let outcome: JobOutcome =
                    serde_json::from_str(job.outcome.as_deref().ok_or(Error::Storage)?)
                        .map_err(|_| Error::Storage)?;
                if !outcome.valid_for(&spec.operation) {
                    return Err(Error::Storage);
                }
                Ok(Some(outcome))
            }
            _ => Err(Error::Storage),
        }
    }

    /// Record a settled release. Only `Completed` discharges the duty; a refusal
    /// returns it to pending, and its publication time holds the next attempt
    /// off for another grace. Release is never forced.
    async fn finish_journal(
        &self,
        tx: &Database,
        app: &AppId,
        deployment: &DeploymentId,
        pending: &str,
        outcome: JobOutcome,
    ) -> Result<(), Error> {
        let resolved = match outcome {
            JobOutcome::Completed {} => JOURNAL_RELEASED,
            JobOutcome::Waiting {} | JobOutcome::Rejected {} => JOURNAL_PENDING,
            JobOutcome::Management { .. } => return Err(Error::Storage),
        };
        changed_once(
            tx.entity::<holds::Entity>()?
                .update_many(
                    identity(app, deployment)?
                        .and(holds::journal_state.eq(JOURNAL_RELEASING)?)
                        .and(holds::journal_job_id.eq(Some(pending))?),
                    holds::journal_state
                        .set(resolved)?
                        .and(holds::journal_job_id.set(None::<&str>)?)?,
                )
                .await?,
        )
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
/// unfinished journal release duties, and holds confirmed no later than `cutoff`
/// that the app's enabled calendar does not select and no unsettled job projects.
/// A released queue hold whose journal duty is still open is a candidate on the
/// same terms, with its latest release publication aged by the same `cutoff`.
/// Selection and use are joined before the limit, so retained holds take no page
/// slot; an unconfirmed hold time is surfaced as damage.
/// `Queue::maintain_deployment` decides each candidate again under its app lock.
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
    let selected = tx
        .entity::<schedule_activations::Entity>()?
        .alias("selected")?;
    let job = tx.entity::<jobs::Entity>()?.alias("job")?;
    let aged = hold
        .column(holds::held_at)
        .is_null()
        .or(hold.column(holds::held_at).lte(Some(cutoff))?);
    let idle = selected
        .column(schedule_activations::id)
        .is_null()
        .and(job.column(jobs::id).is_null());
    let mut filter = hold
        .column(holds::state)
        .eq("acquiring")?
        .or(hold.column(holds::state).eq("releasing")?)
        .or(hold
            .column(holds::state)
            .eq("held")?
            .and(aged.clone())
            .and(idle.clone()))
        // A settled release reply is applied whatever the deployment's age.
        .or(hold
            .column(holds::state)
            .eq("released")?
            .and(hold.column(holds::journal_state).eq(JOURNAL_RELEASING)?))
        .or(hold
            .column(holds::state)
            .eq("released")?
            .and(hold.column(holds::journal_state).eq(JOURNAL_PENDING)?)
            .and(
                hold.column(holds::journal_published_at)
                    .is_null()
                    .or(hold.column(holds::journal_published_at).lte(Some(cutoff))?),
            )
            .and(aged)
            .and(idle));
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
        // Only a held or released row joins its jobs, so no candidate repeats
        // per job. A released hold has no unsettled job projecting it: release
        // required that, and reacquisition leaves the released state.
        .left_join(
            &job,
            job.column(jobs::app_id)
                .eq(hold.column(holds::app_id))?
                .and(
                    job.column(jobs::deployment_id)
                        .eq(hold.column(holds::deployment_id))?,
                )
                .and(job.column(jobs::state).ne("settled")?)
                .and(
                    hold.column(holds::state)
                        .eq("held")?
                        .or(hold.column(holds::state).eq("released")?),
                ),
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
fn changed_once(changed: i64) -> Result<(), Error> {
    if changed == 1 {
        Ok(())
    } else {
        Err(Error::Storage)
    }
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
