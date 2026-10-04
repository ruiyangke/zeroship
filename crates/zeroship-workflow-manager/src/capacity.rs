//! Execution-slot capacity derived from each zone's policy-filtered backlog.
#![expect(
    clippy::future_not_send,
    reason = "capacity operations share the queue's owning compio runtime"
)]

use crate::{
    models::capacity_targets as targets,
    policy::{PolicyObservation, PolicySource},
    scheduling::Backlog,
    Error, Queue,
};
use std::{
    cell::RefCell,
    collections::HashMap,
    fmt::Debug,
    future::Future,
    pin::Pin,
    rc::Rc,
    time::{Duration, Instant},
};
use zeroship_core::{app_id::AppId, workflow_coordination::Revision, zone_id::ZoneId};
use zeroship_data_orm::{
    orm::{Database, Entity, FindOptions, FromRow},
    value,
};

#[derive(Clone, Copy, Debug)]
pub struct Options {
    pub min_slots: i64,
    pub max_slots: i64,
    pub idle_hold_down: Duration,
    pub request_timeout: Duration,
    pub retry_interval: Duration,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            min_slots: 0,
            max_slots: 1024,
            idle_hold_down: Duration::from_secs(300),
            request_timeout: Duration::from_secs(10),
            retry_interval: Duration::from_secs(30),
        }
    }
}

impl Options {
    pub fn validate(&self) -> Result<(), Error> {
        for duration in [
            self.idle_hold_down,
            self.request_timeout,
            self.retry_interval,
        ] {
            i64::try_from(duration.as_millis())
                .ok()
                .filter(|value| *value > 0)
                .ok_or(Error::Invalid)?;
        }
        if self.min_slots < 0 || self.max_slots < 1 || self.min_slots > self.max_slots {
            return Err(Error::Invalid);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    PoolExhausted,
    NoSigner,
    Unavailable,
}

impl Refusal {
    const fn as_str(self) -> &'static str {
        match self {
            Self::PoolExhausted => "pool_exhausted",
            Self::NoSigner => "no_signer",
            Self::Unavailable => "unavailable",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapacityRequest {
    pub zone: ZoneId,
    pub revision: Revision,
    pub desired_slots: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CapacityReply {
    Accepted,
    Refused(Refusal),
}

pub type CapacityFuture<'a> = Pin<Box<dyn Future<Output = Result<CapacityReply, Error>> + 'a>>;

pub trait CapacityProvider: Debug {
    fn ensure<'a>(&'a self, request: &'a CapacityRequest) -> CapacityFuture<'a>;
}

#[derive(Debug, Default, Clone, Copy)]
pub struct LocalCapacity;

impl CapacityProvider for LocalCapacity {
    fn ensure<'a>(&'a self, _request: &'a CapacityRequest) -> CapacityFuture<'a> {
        Box::pin(async { Ok(CapacityReply::Accepted) })
    }
}

#[derive(Debug, Clone, Copy)]
pub struct StaticPool {
    pub pool_slots: u64,
}

impl CapacityProvider for StaticPool {
    fn ensure<'a>(&'a self, request: &'a CapacityRequest) -> CapacityFuture<'a> {
        Box::pin(async move {
            Ok(if request.desired_slots > self.pool_slots {
                CapacityReply::Refused(Refusal::PoolExhausted)
            } else {
                CapacityReply::Accepted
            })
        })
    }
}

/// What one visit of a zone measured.
///
/// `demand` drives the target. The rest is recorded on the target row for
/// operators: the claimable backlog and its oldest row, and the creator work
/// that cannot deliver now, which never counts toward demand.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Census {
    /// Execution slots the zone's admitted work asks for: per app, its live
    /// leases plus as many claimable rows as its `max_running` leaves room for.
    pub demand: i64,
    /// Claimable rows of admitted apps.
    pub backlog: i64,
    /// The earliest `available_at` among those rows.
    pub oldest_available_at: Option<i64>,
    /// Creator rows a claim would take but for the delivery budget.
    pub exhausted: i64,
    /// Creator rows given back and still inside their back-off.
    pub backed_off: i64,
    /// Unsettled creator rows of apps whose policy withholds dispatch, which
    /// another zone owns, or which Control deleted.
    pub withheld: i64,
}

impl Census {
    fn admit(&mut self, running: i64, room: i64, backlog: &Backlog) -> Result<(), Error> {
        let add = |total: i64, value: i64| total.checked_add(value).ok_or(Error::Capacity);
        self.demand = add(self.demand, add(running, backlog.claimable.min(room))?)?;
        self.backlog = add(self.backlog, backlog.claimable)?;
        self.exhausted = add(self.exhausted, backlog.exhausted)?;
        self.backed_off = add(self.backed_off, backlog.backed_off)?;
        self.oldest_available_at = match (self.oldest_available_at, backlog.oldest_available_at) {
            (Some(left), Some(right)) => Some(left.min(right)),
            (left, right) => left.or(right),
        };
        Ok(())
    }
}

/// How far a zone's cycle got by the end of one pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Visit {
    /// The cycle reached the end of the zone: every app with creator work was
    /// measured once, over this pass and the ones before it.
    Complete(Census),
    /// The deadline fell between apps. The census covers the apps the cycle
    /// has measured so far, so it may raise the target and never lowers it;
    /// the next pass continues the cycle after the last of them.
    Partial(Census),
    /// A policy observation was unavailable: nothing is known, nothing moves.
    Frozen,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exchange {
    Idle,
    Applied,
    Stale,
}

#[derive(FromRow)]
#[orm(entity = targets)]
struct TargetRow {
    revision: i64,
    desired: i64,
    state: String,
    retry_at: Option<i64>,
    below_since: Option<i64>,
}

/// Apps one visit reads per page; a cycle pages to the end of the zone.
const SCOPE_PAGE: usize = 256;

/// Where one zone's measurement stands between lane passes.
#[derive(Debug, Clone, Default)]
struct Cycle {
    /// The last app this cycle measured; the next pass continues after it.
    after: Option<AppId>,
    /// What this cycle has measured so far.
    census: Census,
}

#[derive(Debug, Clone)]
pub struct Capacity {
    queue: Queue,
    provider: Rc<dyn CapacityProvider>,
    options: Options,
    /// Each zone's unfinished cycle. Held in this process only: a restart
    /// begins every zone's cycle again, which can delay a decrease and never
    /// causes one.
    cycles: Rc<RefCell<HashMap<ZoneId, Cycle>>>,
}

impl Capacity {
    pub fn new(
        queue: Queue,
        provider: Rc<dyn CapacityProvider>,
        options: Options,
    ) -> Result<Self, Error> {
        options.validate()?;
        queue.database.entity::<targets::Entity>()?;
        Ok(Self {
            queue,
            provider,
            options,
            cycles: Rc::default(),
        })
    }

    /// Measure `zone` until `visit_until`, move its target by the rise and
    /// hold-down rules, and ask the provider when a request is due.
    ///
    /// # Errors
    /// Reports unavailable storage and a provider failure.
    pub async fn reconcile(
        &self,
        zone: &ZoneId,
        policies: &dyn PolicySource,
        visit_until: Instant,
    ) -> Result<Exchange, Error> {
        let (census, complete) = match self.visit(zone, policies, visit_until).await? {
            Visit::Complete(census) => (census, true),
            Visit::Partial(census) => (census, false),
            Visit::Frozen => return Ok(Exchange::Idle),
        };
        let now = self.queue.now().await?;
        let desired = census
            .demand
            .clamp(self.options.min_slots, self.options.max_slots);
        let request = self
            .queue
            .transact(|tx| async move {
                let row = target(&tx, zone).await?.ok_or(Error::Storage)?;
                let mut revision = row.revision;
                let mut applied = row.desired;
                let mut below_since = row.below_since;
                if desired > row.desired {
                    applied = desired;
                    below_since = None;
                } else if desired < row.desired && complete {
                    let since = below_since.get_or_insert(now);
                    let hold = i64::try_from(self.options.idle_hold_down.as_millis())
                        .map_err(|_| Error::Invalid)?;
                    if now.saturating_sub(*since) >= hold {
                        applied = desired;
                        below_since = None;
                    }
                }
                if applied != row.desired {
                    revision = revision.checked_add(1).ok_or(Error::Capacity)?;
                }
                let due = applied != row.desired
                    || row.state == "requesting"
                    || row.retry_at.is_none_or(|retry| retry <= now);
                let state = if due { "requesting" } else { row.state.as_str() };
                let patch = if complete {
                    value!({"revision":revision,"desired":applied,"state":state,
                        "below_since":below_since,"backlog_depth":census.backlog,
                        "oldest_available_at":census.oldest_available_at,
                        "exhausted_jobs":census.exhausted,"backed_off_jobs":census.backed_off,
                        "withheld_jobs":census.withheld})
                } else {
                    value!({"revision":revision,"desired":applied,"state":state,
                        "below_since":below_since})
                };
                tx.collection(targets::Entity::COLLECTION)?
                    .update(value!({"id":zone.as_str(),"revision":row.revision}), patch)
                    .await?;
                if !due || revision <= 0 {
                    return Ok(None);
                }
                Ok(Some(CapacityRequest {
                    zone: zone.clone(),
                    revision: revision.try_into().map_err(|_| Error::Storage)?,
                    desired_slots: u64::try_from(applied).map_err(|_| Error::Storage)?,
                }))
            })
            .await?;
        let Some(request) = request else {
            return Ok(Exchange::Idle);
        };
        let reply = compio::time::timeout(
            self.options.request_timeout,
            self.provider.ensure(&request),
        )
        .await
        .unwrap_or(Ok(CapacityReply::Refused(Refusal::Unavailable)))?;
        let retry = self
            .queue
            .now()
            .await?
            .checked_add(
                i64::try_from(self.options.retry_interval.as_millis())
                    .map_err(|_| Error::Invalid)?,
            )
            .ok_or(Error::Capacity)?;
        self.queue
            .transact(|tx| async move {
                let row = target(&tx, zone).await?.ok_or(Error::Storage)?;
                if row.revision != request.revision.get() {
                    return Ok(Exchange::Stale);
                }
                let (state, refusal) = match reply {
                    CapacityReply::Accepted => ("steady", None),
                    CapacityReply::Refused(reason) => ("refused", Some(reason.as_str())),
                };
                tx.collection(targets::Entity::COLLECTION)?
                    .update(
                        value!({"id":zone.as_str(),"revision":row.revision}),
                        value!({"state":state,"refusal":refusal,"retry_at":retry,
                            "attempt_deadline":null}),
                    )
                    .await?;
                Ok(Exchange::Applied)
            })
            .await
    }

    /// Measure `zone`'s apps through their policy, one app per step, from
    /// where the zone's current cycle stands. No policy lookup happens inside
    /// a transaction, and the deadline is checked between apps, so a pass
    /// always measures at least one.
    ///
    /// ONLY APPS WITH CREATOR WORK ARE PAGED: an app holding an unsettled
    /// creator row, which is what the census counts. An idle scope costs no
    /// read and no policy observation.
    ///
    /// A CYCLE SPANS PASSES. A pass the deadline cuts leaves the cycle's
    /// position and what it measured for the next pass to continue from, so a
    /// zone too large for one pass is covered by successive ones; its census
    /// may raise the target. The pass that reaches the end of the zone
    /// completes the cycle, whose census has measured every app once and may
    /// lower the target too.
    async fn visit(
        &self,
        zone: &ZoneId,
        policies: &dyn PolicySource,
        until: Instant,
    ) -> Result<Visit, Error> {
        let now = self.queue.now().await?;
        let mut cycle = self.cycles.borrow().get(zone).cloned().unwrap_or_default();
        let mut measured = false;
        loop {
            let after = cycle.after.clone();
            let page = self
                .queue
                .transact(|tx| {
                    let after = after.clone();
                    async move {
                        crate::scheduling::apps_with_creator_work_in_zone(
                            &tx,
                            zone,
                            after.as_ref(),
                            SCOPE_PAGE,
                        )
                        .await
                    }
                })
                .await?;
            let full = page.len() == SCOPE_PAGE;
            for app in page {
                if measured && Instant::now() >= until {
                    let census = cycle.census;
                    self.cycles.borrow_mut().insert(zone.clone(), cycle);
                    return Ok(Visit::Partial(census));
                }
                let Ok(observation) = policies.observe(&app).await else {
                    self.cycles.borrow_mut().insert(zone.clone(), cycle);
                    return Ok(Visit::Frozen);
                };
                self.measure(&mut cycle.census, &app, zone, &observation, now)
                    .await?;
                cycle.after = Some(app);
                measured = true;
            }
            if !full {
                self.cycles.borrow_mut().remove(zone);
                return Ok(Visit::Complete(cycle.census));
            }
        }
    }

    /// Add one app's measure to `census`: its demand and backlog when its
    /// policy admits dispatch in `zone`, its unsettled creator rows as
    /// withheld when it does not.
    async fn measure(
        &self,
        census: &mut Census,
        app: &AppId,
        zone: &ZoneId,
        observation: &PolicyObservation,
        now: i64,
    ) -> Result<(), Error> {
        let policy = observation.policy();
        if observation.admits_zone(zone).is_err()
            || !policy.admission
            || !policy.dispatch
            || policy.max_running == 0
        {
            let withheld = self
                .queue
                .transact(|tx| async move {
                    crate::scheduling::unsettled_creator_rows(&tx, app).await
                })
                .await?;
            census.withheld = census
                .withheld
                .checked_add(withheld)
                .ok_or(Error::Capacity)?;
            return Ok(());
        }
        let ceiling = policy.max_delivery_attempts;
        let (running, backlog) = self
            .queue
            .transact(|tx| async move {
                let running = crate::scheduling::live_advance_count(&tx, app, now).await?;
                let backlog = crate::scheduling::backlog(&tx, app, now, ceiling).await?;
                Ok((running, backlog))
            })
            .await?;
        census.admit(running, (policy.max_running - running).max(0), &backlog)
    }
}

async fn target(tx: &Database, zone: &ZoneId) -> Result<Option<TargetRow>, Error> {
    Ok(tx
        .entity::<targets::Entity>()?
        .find::<TargetRow>(
            targets::id.eq(zone.as_str())?,
            FindOptions {
                limit: Some(1),
                ..Default::default()
            },
        )
        .await?
        .into_iter()
        .next())
}
