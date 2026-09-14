//! Capacity for due work that free eligible capacity cannot absorb.
//!
//! The placement lane places an app on free eligible capacity first. An app
//! that still has claimable work and no eligible owner becomes durable demand
//! in its execution zone. Every manager replica therefore computes capacity
//! from the same committed rows rather than from its own partial view.
//!
//! The declarative contract keeps one capacity target per zone, in placement
//! slots: the zone's live placements plus its unplaced demand. Placing an app
//! moves it from one term to the other, so satisfying demand leaves the target
//! unchanged. The target's revision advances only when that number changes,
//! under the zone row's lock. A provider request is claimed in the same
//! transaction and sent outside every lock; its reply applies only to the
//! revision and attempt it answered. Repeating a request after a lost reply is
//! harmless because the target is a value, not an instruction. A lower target
//! applies only after the zone's demand stayed below it for the idle
//! hold-down. Provider failure keeps the demand, the jobs and the target.
//!
//! The intents contract is the comparison: one imperative provisioning intent
//! per owner-less app, fenced by a generation. Its provider must deduplicate
//! by intent identity, because a retried request is another instruction.
#![expect(
    clippy::future_not_send,
    reason = "capacity operations share the queue's owning compio runtime"
)]

use crate::{
    coordinator::{Coordinator, Placed},
    eligibility::ZoneId,
    models::{
        assignments, capacity_demands as demands, capacity_intents as intents,
        capacity_targets as targets, workers,
    },
    queue::lock_scope,
    scheduling, Error,
};
use std::{
    fmt::Debug,
    future::Future,
    pin::Pin,
    rc::Rc,
    time::{Duration, Instant},
};
use zeroship_core::{
    app_id::AppId,
    workflow_coordination::{Assignment, Revision},
};
use zeroship_data_orm::orm::{
    count_rows, ConflictTarget, Database, FromRow, Insertable, Patch,
};

/// Capacity pacing. The hold-down and retry values are operator settings.
#[derive(Clone, Copy, Debug)]
pub struct Options {
    /// A lower target applies only after the zone's computed demand stayed
    /// below the current target for this long.
    pub idle_hold_down: Duration,
    /// Bound on one provider request. An unanswered request is recorded as an
    /// `unavailable` refusal, and another replica may retry after this bound.
    pub request_timeout: Duration,
    /// Pause after any reply before the same target or intent is requested
    /// again while its demand remains unplaced.
    pub retry_interval: Duration,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            idle_hold_down: Duration::from_secs(300),
            request_timeout: Duration::from_secs(10),
            retry_interval: Duration::from_secs(30),
        }
    }
}

impl Options {
    /// # Errors
    /// Rejects empty or unrepresentable durations.
    pub fn validate(&self) -> Result<(), Error> {
        millis(self.idle_hold_down)?;
        millis(self.request_timeout)?;
        millis(self.retry_interval)?;
        if Instant::now().checked_add(self.request_timeout).is_none() {
            return Err(Error::Invalid);
        }
        Ok(())
    }
}

fn millis(value: Duration) -> Result<i64, Error> {
    i64::try_from(value.as_millis())
        .ok()
        .filter(|value| *value > 0)
        .ok_or(Error::Invalid)
}

/// A provider's closed refusal. Every refusal is retryable and durable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// A fixed pool has no more capacity to give; an operator must scale it.
    PoolExhausted,
    /// No active enroller exists in the zone to enroll new workers.
    NoEnroller,
    /// The provider could not be reached or did not answer in time.
    Unavailable,
}

impl Refusal {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::PoolExhausted => "pool_exhausted",
            Self::NoEnroller => "no_enroller",
            Self::Unavailable => "unavailable",
        }
    }

    fn parse(value: &str) -> Result<Self, Error> {
        match value {
            "pool_exhausted" => Ok(Self::PoolExhausted),
            "no_enroller" => Ok(Self::NoEnroller),
            "unavailable" => Ok(Self::Unavailable),
            _ => Err(Error::Storage),
        }
    }
}

/// A zone's declarative target. `ready_slots` is the manager's own observation
/// of ready registered capacity in the zone, for providers that cannot start
/// processes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapacityRequest {
    pub zone: ZoneId,
    pub revision: Revision,
    pub desired_slots: u64,
    pub ready_slots: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapacityReply {
    /// The provider accepted the target and reports the ready capacity it sees.
    Progress { ready_slots: u64 },
    Refused(Refusal),
}

pub type CapacityFuture<'a> = Pin<Box<dyn Future<Output = Result<CapacityReply, Error>> + 'a>>;

/// Applies a zone's declarative capacity target.
///
/// A provider holds only scale authority over worker units in one zone: it
/// never receives creator credentials, secret-mount authority or queue
/// messages. Repeating a request for the same revision must converge on the
/// same capacity.
pub trait CapacityProvider: Debug {
    fn ensure<'a>(&'a self, request: &'a CapacityRequest) -> CapacityFuture<'a>;
}

/// The local host's trusted in-process worker is its capacity, so every
/// target is already satisfied.
#[derive(Debug, Default, Clone, Copy)]
pub struct LocalCapacity;

impl CapacityProvider for LocalCapacity {
    fn ensure<'a>(&'a self, request: &'a CapacityRequest) -> CapacityFuture<'a> {
        Box::pin(async move {
            Ok(CapacityReply::Progress {
                ready_slots: request.ready_slots,
            })
        })
    }
}

/// A fixed pool of workers the deployment starts itself.
///
/// Compose replicas on one host are such a pool. It never starts processes.
/// It reports progress while registered ready capacity covers the target, and
/// otherwise a durable `pool_exhausted` refusal that an operator resolves by
/// scaling the pool.
#[derive(Debug, Default, Clone, Copy)]
pub struct StaticPool;

impl CapacityProvider for StaticPool {
    fn ensure<'a>(&'a self, request: &'a CapacityRequest) -> CapacityFuture<'a> {
        Box::pin(async move {
            Ok(if request.ready_slots >= request.desired_slots {
                CapacityReply::Progress {
                    ready_slots: request.ready_slots,
                }
            } else {
                CapacityReply::Refused(Refusal::PoolExhausted)
            })
        })
    }
}

/// The comparison contract's request: start capacity for one owner-less app.
/// The intent's identity is the app and its generation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProvisionRequest {
    pub app: AppId,
    pub zone: ZoneId,
    pub generation: Revision,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProvisionReply {
    Provisioned,
    Refused(Refusal),
}

pub type ProvisionFuture<'a> = Pin<Box<dyn Future<Output = Result<ProvisionReply, Error>> + 'a>>;

/// Imperative per-app provisioning. Each request is an instruction, so the
/// provider must deduplicate by intent identity to avoid duplicate starts.
pub trait ProvisioningProvider: Debug {
    fn provision<'a>(&'a self, request: &'a ProvisionRequest) -> ProvisionFuture<'a>;
}

/// Which capacity contract a manager runs.
#[derive(Clone, Debug)]
pub enum Contract {
    /// One declarative, revisioned target per execution zone.
    Declarative(Rc<dyn CapacityProvider>),
    /// One imperative provisioning intent per owner-less app.
    Intents(Rc<dyn ProvisioningProvider>),
}

impl Contract {
    #[must_use]
    pub fn declarative(provider: Rc<dyn CapacityProvider>) -> Self {
        Self::Declarative(provider)
    }

    #[must_use]
    pub fn intents(provider: Rc<dyn ProvisioningProvider>) -> Self {
        Self::Intents(provider)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetState {
    /// The provider accepted the current revision.
    Steady,
    /// The current revision has not been accepted yet.
    Requesting,
    /// The provider refused the current revision; it is retried later.
    Refused,
}

impl TargetState {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Steady => "steady",
            Self::Requesting => "requesting",
            Self::Refused => "refused",
        }
    }

    fn parse(value: &str) -> Result<Self, Error> {
        match value {
            "steady" => Ok(Self::Steady),
            "requesting" => Ok(Self::Requesting),
            "refused" => Ok(Self::Refused),
            _ => Err(Error::Storage),
        }
    }
}

/// A zone's durable capacity target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub zone: ZoneId,
    /// Zero until the first target is computed.
    pub revision: i64,
    pub desired: i64,
    pub state: TargetState,
    pub refusal: Option<Refusal>,
    pub observed: Option<i64>,
    /// Provider requests claimed so far, across every revision.
    pub attempt: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IntentState {
    Acquiring,
    Requested,
    Provisioned,
    Refused,
    Settled,
}

impl IntentState {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Acquiring => "acquiring",
            Self::Requested => "requested",
            Self::Provisioned => "provisioned",
            Self::Refused => "refused",
            Self::Settled => "settled",
        }
    }

    fn parse(value: &str) -> Result<Self, Error> {
        match value {
            "acquiring" => Ok(Self::Acquiring),
            "requested" => Ok(Self::Requested),
            "provisioned" => Ok(Self::Provisioned),
            "refused" => Ok(Self::Refused),
            "settled" => Ok(Self::Settled),
            _ => Err(Error::Storage),
        }
    }
}

/// An app's durable provisioning intent under the comparison contract.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Intent {
    pub app: AppId,
    pub zone: ZoneId,
    pub generation: i64,
    pub state: IntentState,
    pub refusal: Option<Refusal>,
    pub attempt: i64,
}

/// What one placement lane visit found for an app.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Visit {
    /// No claimable work, or no live app to place; its demand is cleared.
    Idle,
    /// A ready eligible worker owns the app.
    Owned,
    /// The app was placed on a selected eligible worker.
    Placed(Assignment),
    /// No eligible capacity absorbed the app; its demand is recorded.
    Unplaced(ZoneId),
}

/// A request claimed for one revision and attempt of a zone's target.
struct Claim {
    revision: i64,
    attempt: i64,
    desired: i64,
    ready: i64,
}

/// What one provider exchange did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exchange {
    /// Nothing was due, or the row no longer exists.
    Idle,
    /// The reply applied to the revision or generation it answered.
    Applied,
    /// A newer revision, generation or attempt superseded the reply.
    Stale,
}

#[derive(FromRow)]
#[orm(entity = demands)]
struct DemandRow {
    id: String,
    execution_zone_id: String,
}

#[derive(Insertable)]
#[orm(entity = demands)]
struct NewDemand<'a> {
    id: &'a str,
    execution_zone_id: &'a str,
    recorded_at: i64,
}

#[derive(FromRow, Clone, PartialEq, Eq)]
#[orm(entity = targets)]
struct TargetRow {
    id: String,
    revision: i64,
    desired: i64,
    state: String,
    refusal: Option<String>,
    observed: Option<i64>,
    attempt: i64,
    attempt_deadline: Option<i64>,
    retry_at: Option<i64>,
    below_since: Option<i64>,
}

impl TargetRow {
    fn view(&self) -> Result<Target, Error> {
        if self.revision < 0 || self.desired < 0 || self.attempt < 0 {
            return Err(Error::Storage);
        }
        Ok(Target {
            zone: ZoneId::parse(&self.id)?,
            revision: self.revision,
            desired: self.desired,
            state: TargetState::parse(&self.state)?,
            refusal: self.refusal.as_deref().map(Refusal::parse).transpose()?,
            observed: self.observed,
            attempt: self.attempt,
        })
    }

    /// A new revision for a changed target. An in-flight request for the old
    /// revision stays in flight; its reply will be stale.
    fn advance(&mut self, desired: i64) -> Result<(), Error> {
        self.revision = self.revision.checked_add(1).ok_or(Error::Capacity)?;
        self.desired = desired;
        TargetState::Requesting.as_str().clone_into(&mut self.state);
        self.refusal = None;
        self.attempt_deadline = None;
        self.retry_at = None;
        self.below_since = None;
        Ok(())
    }

    fn patch(&self) -> Result<Patch<targets::Entity>, Error> {
        Ok(targets::revision
            .set(self.revision)?
            .and(targets::desired.set(self.desired)?)?
            .and(targets::state.set(self.state.as_str())?)?
            .and(targets::refusal.set(self.refusal.as_deref())?)?
            .and(targets::observed.set(self.observed)?)?
            .and(targets::attempt.set(self.attempt)?)?
            .and(targets::attempt_deadline.set(self.attempt_deadline)?)?
            .and(targets::retry_at.set(self.retry_at)?)?
            .and(targets::below_since.set(self.below_since)?)?)
    }
}

#[derive(Insertable)]
#[orm(entity = targets)]
struct ZoneIdentity<'a> {
    id: &'a str,
}

#[derive(FromRow, Clone)]
#[orm(entity = intents)]
struct IntentRow {
    id: String,
    execution_zone_id: String,
    generation: i64,
    state: String,
    refusal: Option<String>,
    attempt: i64,
    attempt_deadline: Option<i64>,
    retry_at: Option<i64>,
}

impl IntentRow {
    fn view(&self) -> Result<Intent, Error> {
        if self.generation <= 0 || self.attempt < 0 {
            return Err(Error::Storage);
        }
        Ok(Intent {
            app: AppId::parse(&self.id).map_err(|_| Error::Storage)?,
            zone: ZoneId::parse(&self.execution_zone_id)?,
            generation: self.generation,
            state: IntentState::parse(&self.state)?,
            refusal: self.refusal.as_deref().map(Refusal::parse).transpose()?,
            attempt: self.attempt,
        })
    }
}

#[derive(Insertable)]
#[orm(entity = intents)]
struct NewIntent<'a> {
    id: &'a str,
    execution_zone_id: &'a str,
    generation: i64,
    state: &'a str,
    attempt: i64,
}

/// Placement demand and capacity requests over one platform queue.
#[derive(Debug, Clone)]
pub struct Capacity {
    coordinator: Coordinator,
    contract: Contract,
    options: Options,
}

impl Capacity {
    /// # Errors
    /// Rejects invalid options and incompatible generated model metadata.
    pub fn new(coordinator: Coordinator, contract: Contract, options: Options) -> Result<Self, Error> {
        options.validate()?;
        let database = &coordinator.queue().database;
        database.entity::<demands::Entity>()?;
        database.entity::<targets::Entity>()?;
        database.entity::<intents::Entity>()?;
        Ok(Self {
            coordinator,
            contract,
            options,
        })
    }

    #[must_use]
    pub const fn coordinator(&self) -> &Coordinator {
        &self.coordinator
    }

    #[must_use]
    pub const fn contract(&self) -> &Contract {
        &self.contract
    }

    /// Give an app an owner when it has claimable work. Place it on free
    /// eligible capacity first; record its demand only when none admits it.
    /// An app without claimable work, or without a live Control row, has its
    /// demand cleared.
    ///
    /// # Errors
    /// Reports unavailable eligibility or platform storage.
    pub async fn visit(&self, app: &AppId) -> Result<Visit, Error> {
        let queue = self.coordinator.queue();
        let due = queue
            .transact(|tx| async move {
                let now = queue.clock.now().await?;
                Ok(Box::pin(scheduling::candidate(&tx, app, now))
                    .await?
                    .is_some())
            })
            .await?;
        if !due {
            self.settle(app).await?;
            return Ok(Visit::Idle);
        }
        let visit = match self.coordinator.place(app).await? {
            Placed::Assigned(assignment) => Visit::Placed(assignment),
            Placed::Owned => Visit::Owned,
            Placed::Ineligible => Visit::Idle,
            Placed::Unplaced(zone) => return self.record(app, &zone).await,
        };
        self.settle(app).await?;
        Ok(visit)
    }

    /// Record unplaced demand under the app lock. A placement that committed
    /// after this visit's own attempt already owns the app, so the lock and the
    /// ownership recheck keep a stale visit from recording demand.
    async fn record(&self, app: &AppId, zone: &ZoneId) -> Result<Visit, Error> {
        let queue = self.coordinator.queue();
        queue
            .transact(|tx| async move {
                lock_scope(&tx, app).await?;
                if Box::pin(self.coordinator.has_owner(&tx, app, true)).await? {
                    Box::pin(self.clear_in(&tx, app)).await?;
                    return Ok(Visit::Owned);
                }
                let now = queue.clock.now().await?;
                match &self.contract {
                    Contract::Declarative(_) => {
                        match demand(&tx, app).await? {
                            Some(row) if row.execution_zone_id != zone.as_str() => {
                                return Err(Error::Storage)
                            }
                            Some(_) => {}
                            None => {
                                tx.entity::<demands::Entity>()?
                                    .insert::<_, DemandRow>(NewDemand {
                                        id: app.as_str(),
                                        execution_zone_id: zone.as_str(),
                                        recorded_at: now,
                                    })
                                    .await?;
                            }
                        }
                        let _: TargetRow = tx
                            .entity::<targets::Entity>()?
                            .upsert(
                                ZoneIdentity { id: zone.as_str() },
                                ConflictTarget::new(targets::id),
                            )
                            .await?;
                    }
                    Contract::Intents(_) => match intent(&tx, app).await? {
                        None => {
                            tx.entity::<intents::Entity>()?
                                .insert::<_, IntentRow>(NewIntent {
                                    id: app.as_str(),
                                    execution_zone_id: zone.as_str(),
                                    generation: 1,
                                    state: IntentState::Acquiring.as_str(),
                                    attempt: 0,
                                })
                                .await?;
                        }
                        Some(row) if row.execution_zone_id != zone.as_str() => {
                            return Err(Error::Storage)
                        }
                        Some(row) if row.state == IntentState::Settled.as_str() => {
                            let next = row.generation.checked_add(1).ok_or(Error::Capacity)?;
                            guarded_intent(
                                &tx,
                                &row,
                                intents::generation
                                    .set(next)?
                                    .and(intents::state.set(IntentState::Acquiring.as_str())?)?
                                    .and(intents::refusal.set(None::<&str>)?)?
                                    .and(intents::attempt_deadline.set(None::<i64>)?)?
                                    .and(intents::retry_at.set(None::<i64>)?)?,
                            )
                            .await?;
                        }
                        Some(_) => {}
                    },
                }
                Ok(Visit::Unplaced(zone.clone()))
            })
            .await
    }

    /// Clear an app's demand once it is owned or no longer needs an owner.
    async fn settle(&self, app: &AppId) -> Result<(), Error> {
        let queue = self.coordinator.queue();
        queue
            .transact(|tx| async move {
                // Demand and intents reference the app's queue scope, so an
                // app without one has nothing to clear.
                match lock_scope(&tx, app).await {
                    Ok(()) => Box::pin(self.clear_in(&tx, app)).await,
                    Err(Error::Denied) => Ok(()),
                    Err(error) => Err(error),
                }
            })
            .await
    }

    async fn clear_in(&self, tx: &Database, app: &AppId) -> Result<(), Error> {
        match &self.contract {
            Contract::Declarative(_) => {
                tx.entity::<demands::Entity>()?
                    .delete_many(demands::id.eq(app.as_str())?)
                    .await?;
            }
            Contract::Intents(_) => {
                if let Some(row) = intent(tx, app).await? {
                    if row.state != IntentState::Settled.as_str() {
                        guarded_intent(
                            tx,
                            &row,
                            intents::state
                                .set(IntentState::Settled.as_str())?
                                .and(intents::attempt_deadline.set(None::<i64>)?)?,
                        )
                        .await?;
                    }
                }
            }
        }
        Ok(())
    }

    /// Recompute a zone's declarative target under its row lock, advance the
    /// revision only when the desired slots changed, and send at most one
    /// claimed request outside every lock. The reply applies only to the
    /// revision and attempt it answered.
    ///
    /// # Errors
    /// Refuses the comparison contract and reports unavailable storage.
    pub async fn reconcile(&self, zone: &ZoneId) -> Result<Exchange, Error> {
        let Contract::Declarative(provider) = &self.contract else {
            return Err(Error::Invalid);
        };
        let Some(claim) = Box::pin(self.claim_target(zone)).await? else {
            return Ok(Exchange::Idle);
        };
        let request = CapacityRequest {
            zone: zone.clone(),
            revision: Revision::try_from(claim.revision).map_err(|_| Error::Storage)?,
            desired_slots: u64::try_from(claim.desired).map_err(|_| Error::Storage)?,
            ready_slots: u64::try_from(claim.ready).map_err(|_| Error::Storage)?,
        };
        // A failed or unanswered provider leaves a durable, retryable refusal.
        let reply = match compio::time::timeout(
            self.options.request_timeout,
            provider.ensure(&request),
        )
        .await
        {
            Ok(Ok(reply)) => reply,
            Ok(Err(_)) | Err(_) => CapacityReply::Refused(Refusal::Unavailable),
        };
        Box::pin(self.apply_target(zone, &claim, reply)).await
    }

    /// Under the zone lock: compute the target from committed placements and
    /// demand, advance its revision if it changed, and claim a request if one
    /// is due and none is in flight.
    async fn claim_target(&self, zone: &ZoneId) -> Result<Option<Claim>, Error> {
        let queue = self.coordinator.queue();
        let hold_down = millis(self.options.idle_hold_down)?;
        let timeout = millis(self.options.request_timeout)?;
        queue
            .transact(|tx| async move {
                if !lock_zone(&tx, zone).await? {
                    return Ok(None);
                }
                let stored = target(&tx, zone).await?.ok_or(Error::Storage)?;
                stored.view()?;
                let now = queue.clock.now().await?;
                let unplaced = tx
                    .entity::<demands::Entity>()?
                    .count(demands::execution_zone_id.eq(zone.as_str())?)
                    .await?;
                let computed = occupied(&tx, zone, now)
                    .await?
                    .checked_add(unplaced)
                    .ok_or(Error::Capacity)?;
                let mut next = stored.clone();
                if computed > stored.desired || (computed > 0 && stored.revision == 0) {
                    next.advance(computed)?;
                } else if computed < stored.desired {
                    match stored.below_since {
                        None => next.below_since = Some(now),
                        Some(since) if now.saturating_sub(since) >= hold_down => {
                            next.advance(computed)?;
                        }
                        Some(_) => {}
                    }
                } else {
                    next.below_since = None;
                }
                // One claimed request at a time. A new revision is sent at once;
                // a refused one, or an accepted one whose demand is still
                // unplaced, is retried after its pacing interval.
                let paced = next.retry_at.is_none_or(|at| at <= now);
                let due = next.revision > 0
                    && next.attempt_deadline.is_none_or(|deadline| deadline <= now)
                    && match TargetState::parse(&next.state)? {
                        TargetState::Requesting => true,
                        TargetState::Refused => paced,
                        TargetState::Steady => unplaced > 0 && paced,
                    };
                let claim = if due {
                    next.attempt = next.attempt.checked_add(1).ok_or(Error::Capacity)?;
                    next.attempt_deadline =
                        Some(now.checked_add(timeout).ok_or(Error::Capacity)?);
                    Some(Claim {
                        revision: next.revision,
                        attempt: next.attempt,
                        desired: next.desired,
                        ready: ready_slots(&tx, zone, now).await?,
                    })
                } else {
                    None
                };
                if next != stored {
                    guarded_target(&tx, &stored, next.patch()?).await?;
                }
                Ok(claim)
            })
            .await
    }

    /// Under the zone lock: record a reply only for the revision and attempt
    /// it answered.
    async fn apply_target(
        &self,
        zone: &ZoneId,
        claim: &Claim,
        reply: CapacityReply,
    ) -> Result<Exchange, Error> {
        let queue = self.coordinator.queue();
        let retry = millis(self.options.retry_interval)?;
        queue
            .transact(|tx| async move {
                if !lock_zone(&tx, zone).await? {
                    return Err(Error::Storage);
                }
                let stored = target(&tx, zone).await?.ok_or(Error::Storage)?;
                if stored.revision != claim.revision || stored.attempt != claim.attempt {
                    return Ok(Exchange::Stale);
                }
                let now = queue.clock.now().await?;
                let mut next = stored.clone();
                next.attempt_deadline = None;
                next.retry_at = Some(now.checked_add(retry).ok_or(Error::Capacity)?);
                match reply {
                    CapacityReply::Progress { ready_slots } => {
                        TargetState::Steady.as_str().clone_into(&mut next.state);
                        next.refusal = None;
                        next.observed =
                            Some(i64::try_from(ready_slots).map_err(|_| Error::Invalid)?);
                    }
                    CapacityReply::Refused(refusal) => {
                        TargetState::Refused.as_str().clone_into(&mut next.state);
                        next.refusal = Some(refusal.as_str().to_owned());
                    }
                }
                guarded_target(&tx, &stored, next.patch()?).await?;
                Ok(Exchange::Applied)
            })
            .await
    }

    /// Request provisioning for one app's intent under the comparison
    /// contract. The reply applies only to the generation and attempt it
    /// answered.
    ///
    /// # Errors
    /// Refuses the declarative contract and reports unavailable storage.
    pub async fn request(&self, app: &AppId) -> Result<Exchange, Error> {
        let Contract::Intents(provider) = &self.contract else {
            return Err(Error::Invalid);
        };
        let queue = self.coordinator.queue();
        let timeout = millis(self.options.request_timeout)?;
        let claim = queue
            .transact(|tx| async move {
                // Lock before reading: every write transaction here starts with
                // its lock, so embedded storage never upgrades a read snapshot.
                lock_scope(&tx, app).await?;
                let Some(stored) = intent(&tx, app).await? else {
                    return Ok(None);
                };
                let view = stored.view()?;
                let now = queue.clock.now().await?;
                let due = stored.attempt_deadline.is_none_or(|deadline| deadline <= now)
                    && match view.state {
                        IntentState::Acquiring | IntentState::Requested => true,
                        IntentState::Refused => stored.retry_at.is_none_or(|at| at <= now),
                        IntentState::Provisioned | IntentState::Settled => false,
                    };
                if !due {
                    return Ok(None);
                }
                let attempt = stored.attempt.checked_add(1).ok_or(Error::Capacity)?;
                guarded_intent(
                    &tx,
                    &stored,
                    intents::attempt
                        .set(attempt)?
                        .and(intents::attempt_deadline.set(Some(now.checked_add(timeout).ok_or(Error::Capacity)?))?)?
                        .and(intents::state.set(IntentState::Requested.as_str())?)?,
                )
                .await?;
                Ok(Some((view.zone, stored.generation, attempt)))
            })
            .await?;
        let Some((zone, generation, attempt)) = claim else {
            return Ok(Exchange::Idle);
        };
        let request = ProvisionRequest {
            app: app.clone(),
            zone,
            generation: Revision::try_from(generation).map_err(|_| Error::Storage)?,
        };
        let reply = match compio::time::timeout(self.options.request_timeout, provider.provision(&request)).await {
            Ok(Ok(reply)) => reply,
            Ok(Err(_)) | Err(_) => ProvisionReply::Refused(Refusal::Unavailable),
        };
        let retry = millis(self.options.retry_interval)?;
        queue
            .transact(|tx| async move {
                lock_scope(&tx, app).await?;
                let stored = intent(&tx, app).await?.ok_or(Error::Storage)?;
                if stored.generation != generation || stored.attempt != attempt {
                    return Ok(Exchange::Stale);
                }
                let now = queue.clock.now().await?;
                let patch = match reply {
                    ProvisionReply::Provisioned => intents::state
                        .set(IntentState::Provisioned.as_str())?
                        .and(intents::refusal.set(None::<&str>)?)?,
                    ProvisionReply::Refused(refusal) => intents::state
                        .set(IntentState::Refused.as_str())?
                        .and(intents::refusal.set(Some(refusal.as_str()))?)?
                        .and(intents::retry_at.set(Some(now.checked_add(retry).ok_or(Error::Capacity)?))?)?,
                };
                guarded_intent(&tx, &stored, patch.and(intents::attempt_deadline.set(None::<i64>)?)?)
                    .await?;
                Ok(Exchange::Applied)
            })
            .await
    }

    /// Read a zone's durable target.
    ///
    /// # Errors
    /// Reports unavailable or malformed platform storage.
    pub async fn target(&self, zone: &ZoneId) -> Result<Option<Target>, Error> {
        self.coordinator
            .queue()
            .transact(|tx| async move { target(&tx, zone).await?.map(|row| row.view()).transpose() })
            .await
    }

    /// The apps recorded as unplaced demand in a zone, in identity order.
    ///
    /// # Errors
    /// Reports unavailable or malformed platform storage.
    pub async fn demands(&self, zone: &ZoneId) -> Result<Vec<AppId>, Error> {
        self.coordinator
            .queue()
            .transact(|tx| async move {
                tx.entity::<demands::Entity>()?
                    .query()
                    .filter(demands::execution_zone_id.eq(zone.as_str())?)
                    .order_by(demands::id.asc())
                    .limit(zeroship_data_orm::sql::MAX_ROW_LIMIT)?
                    .all::<DemandRow>()
                    .await?
                    .into_iter()
                    .map(|row| AppId::parse(&row.id).map_err(|_| Error::Storage))
                    .collect()
            })
            .await
    }

    /// Read an app's durable provisioning intent.
    ///
    /// # Errors
    /// Reports unavailable or malformed platform storage.
    pub async fn intent(&self, app: &AppId) -> Result<Option<Intent>, Error> {
        self.coordinator
            .queue()
            .transact(|tx| async move { intent(&tx, app).await?.map(|row| row.view()).transpose() })
            .await
    }
}

async fn demand(tx: &Database, app: &AppId) -> Result<Option<DemandRow>, Error> {
    Ok(tx
        .entity::<demands::Entity>()?
        .query()
        .filter(demands::id.eq(app.as_str())?)
        .first::<DemandRow>()
        .await?)
}

async fn target(tx: &Database, zone: &ZoneId) -> Result<Option<TargetRow>, Error> {
    Ok(tx
        .entity::<targets::Entity>()?
        .query()
        .filter(targets::id.eq(zone.as_str())?)
        .first::<TargetRow>()
        .await?)
}

async fn intent(tx: &Database, app: &AppId) -> Result<Option<IntentRow>, Error> {
    Ok(tx
        .entity::<intents::Entity>()?
        .query()
        .filter(intents::id.eq(app.as_str())?)
        .first::<IntentRow>()
        .await?)
}

/// The zone row's guarded no-op update: its lock serializes every change to
/// the zone's target. False when the zone has no target yet.
async fn lock_zone(tx: &Database, zone: &ZoneId) -> Result<bool, Error> {
    match tx
        .entity::<targets::Entity>()?
        .update_many(
            targets::id.eq(zone.as_str())?,
            targets::lock_version.increment(0)?,
        )
        .await?
    {
        1 => Ok(true),
        0 => Ok(false),
        _ => Err(Error::Storage),
    }
}

async fn guarded_target(
    tx: &Database,
    stored: &TargetRow,
    patch: Patch<targets::Entity>,
) -> Result<(), Error> {
    let changed = tx
        .entity::<targets::Entity>()?
        .update_many(
            targets::id
                .eq(stored.id.as_str())?
                .and(targets::revision.eq(stored.revision)?)
                .and(targets::attempt.eq(stored.attempt)?),
            patch,
        )
        .await?;
    if changed != 1 {
        return Err(Error::Storage);
    }
    Ok(())
}

async fn guarded_intent(
    tx: &Database,
    stored: &IntentRow,
    patch: Patch<intents::Entity>,
) -> Result<(), Error> {
    let changed = tx
        .entity::<intents::Entity>()?
        .update_many(
            intents::id
                .eq(stored.id.as_str())?
                .and(intents::generation.eq(stored.generation)?)
                .and(intents::attempt.eq(stored.attempt)?),
            patch,
        )
        .await?;
    if changed != 1 {
        return Err(Error::Storage);
    }
    Ok(())
}

/// Live placements on live registrations recorded in the zone.
async fn occupied(tx: &Database, zone: &ZoneId, now: i64) -> Result<i64, Error> {
    let placement = tx.entity::<assignments::Entity>()?.alias("placement")?;
    let worker = tx.entity::<workers::Entity>()?.alias("worker")?;
    let counts = tx
        .from(&placement)
        .inner_join(
            &worker,
            placement
                .column(assignments::worker_id)
                .eq(worker.column(workers::id))?,
        )?
        .filter(
            worker
                .column(workers::execution_zone_id)
                .eq(Some(zone.as_str()))?
                .and(placement.column(assignments::released).eq(false)?)
                .and(placement.column(assignments::expires_at).gt(now)?)
                .and(worker.column(workers::expires_at).gt(now)?),
        )
        .select(count_rows())?
        .all()
        .await?;
    counts
        .first()
        .copied()
        .filter(|count| *count >= 0)
        .ok_or(Error::Storage)
}

/// Ready registered capacity in the zone, in placement slots.
async fn ready_slots(tx: &Database, zone: &ZoneId, now: i64) -> Result<i64, Error> {
    let worker = tx.entity::<workers::Entity>()?.alias("worker")?;
    let sums = tx
        .from(&worker)
        .filter(
            worker
                .column(workers::execution_zone_id)
                .eq(Some(zone.as_str()))?
                .and(worker.column(workers::state).eq("ready")?)
                .and(worker.column(workers::expires_at).gt(now)?),
        )
        .select(worker.column(workers::capacity).sum())?
        .all()
        .await?;
    Ok(sums.first().copied().flatten().unwrap_or(0).max(0))
}
