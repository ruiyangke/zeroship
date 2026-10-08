//! Zone-scoped batch claims and enrolled delivery operations.

use super::Coordinator;
use crate::{
    policy::PolicySource,
    queue::{bounded, Budget, Claimed, ScopeLock},
    Claimant, DeliveryGrant, Error, GiveBack,
};
use std::{
    cell::Cell,
    collections::{hash_map::DefaultHasher, VecDeque},
    future::Future,
    hash::{Hash, Hasher},
    time::{Duration, Instant},
};
use zeroship_core::{
    app_id::AppId,
    workflow_coordination::WorkerId,
    workflow_jobs::{ClaimJobs, Delivery, JournalSettlement, SettlementReceipt},
    zone_id::ZoneId,
};

/// The part of a caller's wait a batch claim leaves unspent, as a divisor of
/// that wait.
///
/// The caller anchors `wait_ms` before it sends, so its wait covers the
/// request's transit, this service's work, building the reply and the reply's
/// transit. This service measures only its own work, from the request's
/// arrival; the two transits and the reply are what the reserve is for, and the
/// in-flight attempt adds nothing to it because every per-app step is cut at the
/// batch deadline. Neither transit is observable here, while the caller chose its
/// wait knowing its own network, so the reserve is a fixed share of that wait
/// rather than a span this service would have to guess: a caller that waits
/// longer is given a larger reserve, and a short wait still leaves the batch the
/// larger part of it.
const REPLY_RESERVE_DIVISOR: u32 = 4;

/// Why a zone claim passed an app it visited.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClaimSkipReason {
    /// The app is deleted, or its frozen zone is not the caller's.
    Denied,
    /// Its policy admits no dispatch: admission or dispatch is off, or it may
    /// run nothing.
    PolicyOff,
    /// It already runs as many deliveries as its policy allows.
    AtCap,
    /// Its policy observation, its storage, its lock or a check of the
    /// caller's enrollment could not be had within the batch's budget.
    Unavailable,
    /// Another transaction holds its queue lock, and the claim passed it without
    /// waiting.
    Contended,
    /// Nothing in its queue is deliverable to a worker now: every claimable row
    /// is exhausted, barrier-blocked or waiting on its occurrence.
    NoCandidate,
    /// The host deferred the committed delivery, and its row went back.
    Deferred,
    /// The host could not accept the committed delivery, and its row went back.
    Refused,
    /// The host's reply had no room for the committed delivery: its row went
    /// back unsent and the batch ended.
    Full,
    /// Any other failure of this app's attempt.
    Failed(Error),
}

impl ClaimSkipReason {
    /// The skip a failed per-app step records.
    ///
    /// A step cut by its share of the batch budget and one whose storage was
    /// unavailable are the same answer to the caller: the app could not be had
    /// this time. That is also what a lock wait on SQLite ends as, so only the
    /// non-waiting lock of PostgreSQL ever reports `Contended`.
    const fn of(error: Error) -> Self {
        match error {
            Error::Contended => Self::Contended,
            Error::Unavailable | Error::Timeout => Self::Unavailable,
            Error::Invalid
            | Error::Denied
            | Error::Conflict
            | Error::Capacity
            | Error::Storage => Self::Failed(error),
        }
    }
}

/// One app a zone claim passed, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClaimSkip {
    pub app_id: AppId,
    pub reason: ClaimSkipReason,
}

/// What a zone claim passed, in visiting order, for logs and contracts.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ClaimReport {
    pub skipped: Vec<ClaimSkip>,
    /// Apps whose grant its host turned away and the claim did not return,
    /// with why: a give-back the queue refused, under that refusal, or a grant
    /// whose lease its admission spent, which is never offered a give-back
    /// whatever the admission answered, as `Error::Timeout`. Each row stays
    /// leased until its lease lapses, and that lapse redelivers it.
    pub lapsing: Vec<(AppId, Error)>,
}

impl ClaimReport {
    fn skip(&mut self, app_id: AppId, reason: ClaimSkipReason) {
        self.skipped.push(ClaimSkip { app_id, reason });
    }
}

/// One worker's batch claim, as its host verified it.
#[derive(Debug, Clone, Copy)]
pub struct ZoneClaim<'a> {
    /// The verified caller.
    pub worker: &'a WorkerId,
    /// The caller's frozen zone, taken from the credential that verified it.
    pub zone: &'a ZoneId,
    /// What the caller asked for.
    pub request: &'a ClaimJobs,
    /// When the batch stops: [`Coordinator::claim_deadline`].
    pub deadline: ClaimDeadline,
}

/// The two instants one batch claim works to, both inside the caller's wait.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClaimDeadline {
    /// No per-app attempt starts after this, and every step of one - the
    /// policy observation, the lock-free reads, the claim transaction and the
    /// host's admission - ends by it.
    pub attempts: Instant,
    /// Every give-back the claim makes - of a live grant its host turned away -
    /// ends by this. A give-back that cannot finish by then leaves its row to
    /// lapse, which redelivers it.
    pub give_backs: Instant,
}

impl ClaimDeadline {
    /// One instant for both, for a caller that keeps nothing back for its
    /// give-backs.
    #[must_use]
    pub const fn at(instant: Instant) -> Self {
        Self {
            attempts: instant,
            give_backs: instant,
        }
    }
}

/// What a claim's host made of one grant the claim committed.
#[derive(Debug)]
pub enum Admission<T> {
    /// Hand the grant to the caller, with what the host attached to it.
    Deliver(T),
    /// Return the grant's row to the queue as `defer` says, recording `reason`,
    /// while its lease is live; a spent one is left to lapse.
    GiveBack {
        reason: ClaimSkipReason,
        defer: GiveBack,
    },
    /// The host can take nothing more in this reply: return the grant's row as
    /// [`GiveBack::Unsent`] while its lease is live, leaving a spent one to
    /// lapse, and end the batch either way, with the cursor before this app so
    /// the next claim begins with it.
    Full,
}

/// The grants a zone claim committed and its host admitted.
#[derive(Debug)]
pub struct ClaimBatch<T> {
    /// In claim order, each with what its host attached.
    pub grants: Vec<(DeliveryGrant, T)>,
    /// Where the next claim continues: after the last app this claim visited,
    /// or before the app whose grant went back unsent from a full reply. The
    /// caller's own cursor when the claim visited nothing.
    pub after: Option<AppId>,
    /// The claim's last lap went all the way round the zone's candidate apps
    /// and delivered nothing, so a caller holding free slots has nothing to gain
    /// by asking again at once.
    pub lap_complete: bool,
}

/// Where a lap stops after it wraps to the start of the zone.
#[derive(Debug, Clone)]
enum Bound {
    /// Through this app: the cursor the lap started after.
    Through(AppId),
    /// Up to this app: the app a lap from no cursor started at.
    Before(AppId),
}

impl Bound {
    /// Byte order, the order the candidate page is read in.
    fn admits(&self, app: &AppId) -> bool {
        match self {
            Self::Through(last) => app.as_str() <= last.as_str(),
            Self::Before(first) => app.as_str() < first.as_str(),
        }
    }
}

/// One lap over the zone's candidate apps: from just after its cursor to the
/// end of the zone, then from the start of the zone back round to the cursor,
/// so each app with claimable work is visited once.
///
/// A lap from no cursor starts at an app of its first page chosen by a hash of
/// the worker, so workers that start together spread over the zone instead of
/// all beginning at its lowest app.
#[derive(Debug)]
struct Lap {
    /// `None` until a lap from no cursor reads its first page.
    bound: Option<Bound>,
    /// The page read continues after this app; `None` reads from the start.
    after: Option<AppId>,
    /// Apps read and not yet visited, in order.
    page: VecDeque<AppId>,
    /// Nothing follows the last page read in the current pass.
    drained: bool,
    /// The lap is on its second pass, from the start of the zone.
    wrapped: bool,
}

impl Lap {
    fn new(cursor: Option<AppId>) -> Self {
        Self {
            bound: cursor.clone().map(Bound::Through),
            after: cursor,
            page: VecDeque::new(),
            drained: false,
            wrapped: false,
        }
    }

    /// The lap's next app, or `None` once it has gone all the way round.
    async fn next(
        &mut self,
        coordinator: &Coordinator,
        claim: &ZoneClaim<'_>,
    ) -> Result<Option<AppId>, Error> {
        loop {
            if let Some(app) = self.page.pop_front() {
                if self.wrapped && !self.bound.as_ref().is_some_and(|bound| bound.admits(&app)) {
                    self.page.clear();
                    return Ok(None);
                }
                return Ok(Some(app));
            }
            if self.drained {
                if self.wrapped || self.bound.is_none() {
                    return Ok(None);
                }
                self.wrapped = true;
                self.drained = false;
                self.after = None;
                continue;
            }
            let page = coordinator.page(claim, self.after.as_ref()).await?;
            self.drained = page.len() < coordinator.options.batch_limit;
            self.after = page.last().cloned().or_else(|| self.after.take());
            if self.bound.is_some() {
                self.page = page.into();
                continue;
            }
            if page.is_empty() {
                return Ok(None);
            }
            let mut hash = DefaultHasher::new();
            claim.worker.hash(&mut hash);
            let span = u64::try_from(page.len()).map_err(|_| Error::Invalid)?;
            let start = usize::try_from(hash.finish() % span).map_err(|_| Error::Invalid)?;
            self.bound = Some(Bound::Before(page[start].clone()));
            self.page = page.into_iter().skip(start).collect();
        }
    }
}

impl Coordinator {
    /// When a batch claim that arrived at `arrival` stops.
    ///
    /// Attempts stop at the claim's budget or at the part of the caller's wait
    /// the reply does not need, whichever ends first. Give-backs stop half the
    /// reply's reserve later: a grant whose admission the attempt deadline cut
    /// still has time to go back, and the other half of the reserve is left
    /// for building the reply and both transits. So nothing the claim does,
    /// give-backs included, keeps the reply past the caller's wait.
    ///
    /// Taken once, from the instant the request arrived, so the time the host
    /// spends authenticating and reading the body is spent from the same budget.
    ///
    /// # Errors
    /// Refuses a deadline the monotonic clock cannot represent.
    pub fn claim_deadline(
        &self,
        arrival: Instant,
        request: &ClaimJobs,
    ) -> Result<ClaimDeadline, Error> {
        let wait = Duration::from_millis(request.wait_ms.get());
        let reserve = wait / REPLY_RESERVE_DIVISOR;
        let attempts = arrival
            .checked_add(wait.saturating_sub(reserve).min(self.options.claim_budget))
            .ok_or(Error::Invalid)?;
        let give_backs = attempts.checked_add(reserve / 2).ok_or(Error::Invalid)?;
        Ok(ClaimDeadline {
            attempts,
            give_backs,
        })
    }

    /// Claim executable work across the caller's zone, at most once per app in
    /// each lap, and hand each committed grant to `admit` before the next app is
    /// tried.
    ///
    /// THE DEADLINE BOUNDS EVERY STEP. No per-app attempt starts after
    /// `claim.deadline.attempts`, and each step of one -- the policy
    /// observation, the reads that pass an app without its lock, the claim
    /// transaction and the host's admission -- runs under the smaller of its
    /// own budget and what remains of the batch. A grant whose admission the
    /// deadline cuts goes back to the queue by `claim.deadline.give_backs`, so
    /// the reply leaves within the caller's wait.
    ///
    /// AN APP IS PASSED WITHOUT ITS LOCK whenever a read can tell it has nothing
    /// to give: a foreign zone or a deletion, policy that admits no dispatch, no
    /// row deliverable under its delivery budget and barriers, or as many live
    /// deliveries as its policy runs at once. Only an app that passes all of
    /// those is locked, on PostgreSQL without waiting, and its concurrency cap
    /// is counted again under that lock, because another worker's claim may
    /// have leased its last slot since the lock-free count.
    ///
    /// A PER-APP FAILURE IS A SKIP. It is recorded in the report and the cursor
    /// still moves past that app, because an error reply would strand every
    /// grant this batch already committed. That includes a transient failure
    /// to verify the caller's enrollment. An error is reserved for what fails
    /// the request itself: an invalid request, an enrollment refused outright,
    /// and a page read before any app was visited. A page read that fails
    /// later ends the batch with what it holds.
    ///
    /// `admit` is the host's half of each grant, for example its journal's
    /// acceptance. A grant it gives back returns to the queue under that
    /// deferral, by the give-back deadline and without waiting for the app's
    /// lock; one that cannot leaves its row to lapse. A host whose reply has
    /// no room left answers [`Admission::Full`]: that grant goes back unsent
    /// and the batch ends before the app, so the next claim begins with it.
    /// A grant whose lease its admission spent is never given back, whatever
    /// the admission answered: it is reported lapsing and its row lapses.
    ///
    /// NOTHING IS SIZED FROM THE CALLER'S NUMBERS. A request above
    /// [`ClaimJobs::MAX_DELIVERIES`] or [`ClaimJobs::MAX_EXCLUDE`] is refused
    /// before any work, and the batch grows by the grants it commits.
    ///
    /// # Errors
    /// Refuses a request above [`ClaimJobs::MAX_DELIVERIES`] deliveries or
    /// [`ClaimJobs::MAX_EXCLUDE`] exclusions, refused enrollment and a page
    /// read that failed before any app was visited.
    pub async fn claim_in_zone<T, F, Fut, A, AFut>(
        &self,
        claim: &ZoneClaim<'_>,
        policies: &dyn PolicySource,
        authorize: F,
        mut admit: A,
    ) -> Result<(ClaimBatch<T>, ClaimReport), Error>
    where
        F: Fn() -> Fut,
        Fut: Future<Output = Result<WorkerId, Error>>,
        A: FnMut(DeliveryGrant) -> AFut,
        AFut: Future<Output = Admission<T>>,
    {
        if claim.request.max.get() > ClaimJobs::MAX_DELIVERIES
            || claim.request.exclude.len() > ClaimJobs::MAX_EXCLUDE
        {
            return Err(Error::Invalid);
        }
        let wanted = usize::try_from(claim.request.max.get()).map_err(|_| Error::Invalid)?;
        // A refusal of the caller's enrollment, from any step, fails the
        // request: a revoked worker can renew nothing this batch committed. An
        // enrollment that could not be checked this time is that step's
        // failure alone, and the grants already committed still go out.
        let refused = &Cell::new(None);
        let (worker, authorize) = (claim.worker, &authorize);
        let authorized = move || async move {
            let result = enrolled(worker, authorize).await;
            if result == Err(Error::Denied) {
                refused.set(Some(Error::Denied));
            }
            result
        };
        authorized().await?;
        let mut batch = ClaimBatch {
            grants: Vec::new(),
            after: claim.request.after.clone(),
            lap_complete: false,
        };
        let mut report = ClaimReport::default();
        let mut lap = Lap::new(batch.after.clone());
        let mut delivered_in_lap = false;
        let mut visited = false;
        while batch.grants.len() < wanted && Instant::now() < claim.deadline.attempts {
            let app = match lap.next(self, claim).await {
                Ok(Some(app)) => app,
                Ok(None) => {
                    if !delivered_in_lap {
                        batch.lap_complete = true;
                        break;
                    }
                    lap = Lap::new(batch.after.clone());
                    delivered_in_lap = false;
                    continue;
                }
                // Nothing visited yet: the request made no progress to keep.
                Err(error) if !visited => return Err(error),
                Err(_) => break,
            };
            visited = true;
            let previous = batch.after.replace(app.clone());
            match self
                .visit(claim, &app, policies, &authorized, &mut admit, &mut report)
                .await
            {
                Ok(grant) => {
                    batch.grants.push(grant);
                    delivered_in_lap = true;
                }
                Err(skip) => {
                    if let Some(error) = refused.get() {
                        return Err(error);
                    }
                    report.skip(app, skip);
                    if skip == ClaimSkipReason::Full {
                        batch.after = previous;
                        break;
                    }
                }
            }
        }
        Ok((batch, report))
    }

    /// One app's attempt: the lock-free checks, the claim transaction and the
    /// host's admission.
    async fn visit<T, Auth, AuthFut, A, AFut>(
        &self,
        claim: &ZoneClaim<'_>,
        app: &AppId,
        policies: &dyn PolicySource,
        authorized: &Auth,
        admit: &mut A,
        report: &mut ClaimReport,
    ) -> Result<(DeliveryGrant, T), ClaimSkipReason>
    where
        Auth: Fn() -> AuthFut,
        AuthFut: Future<Output = Result<WorkerId, Error>>,
        A: FnMut(DeliveryGrant) -> AFut,
        AFut: Future<Output = Admission<T>>,
    {
        let deadline = claim.deadline.attempts;
        let observation = bounded(Budget::at(deadline), policies.observe(app))
            .await
            .and_then(|observed| observed)
            .map_err(|_| ClaimSkipReason::Unavailable)?;
        if observation.admits_zone(claim.zone).is_err() {
            return Err(ClaimSkipReason::Denied);
        }
        let policy = observation.policy();
        if !policy.admission || !policy.dispatch || policy.max_running == 0 {
            return Err(ClaimSkipReason::PolicyOff);
        }
        self.passable(app, policy.max_delivery_attempts, policy.max_running, deadline)
            .await?;
        let grant = match self
            .queue
            .claim_within(
                self.share(deadline),
                ScopeLock::Skip,
                app,
                claim.worker,
                Claimant::Worker,
                Ok(policy.max_delivery_attempts),
                Some(policy.max_running),
                |_| authorized(),
            )
            .await
            .map_err(ClaimSkipReason::of)?
        {
            Claimed::Granted(grant) => *grant,
            Claimed::Empty => return Err(ClaimSkipReason::NoCandidate),
            Claimed::AtCap => return Err(ClaimSkipReason::AtCap),
        };
        let admitted = bounded(Budget::at(deadline), admit(grant.clone()))
            .await
            .unwrap_or(Admission::GiveBack {
                reason: ClaimSkipReason::Unavailable,
                defer: GiveBack::Backoff,
            });
        let (reason, defer) = match admitted {
            Admission::Deliver(attached) => return Ok((grant, attached)),
            Admission::GiveBack { reason, defer } => (reason, defer),
            Admission::Full => (ClaimSkipReason::Full, GiveBack::Unsent),
        };
        // A SPENT GRANT IS LEFT TO LAPSE, whatever its admission answered. The
        // grant's own clock ends before the deadline the queue stored: each
        // clock sample anchors it at the instant the sample was requested,
        // before the database read its clock, less the sample's resolution,
        // and a later sample can only bring it earlier. So a give-back now
        // would race the lease it returns - landing under the admission's
        // deferral while the stored deadline is ahead, refused once it has
        // passed - and a host whose admission is bounded by the grant, as a
        // journal attempt is, ends exactly here every time it runs the lease
        // out. The lapse decides the same way every time: claimable once the
        // stored deadline passes, no pause, no back-off counted, and a journal
        // task accepted under the grant, bounded by the same lease, lapses
        // with it.
        //
        // A grant with lease left is given back, and that give-back is an
        // ordinary queue transaction: like any holder's give-back near the end
        // of its lease, it is refused if the stored deadline passes first, and
        // the row lapses. Holding back a reserve of lease would not remove
        // that boundary, only move it, and would lapse grants that could have
        // gone back.
        if let Err(error) = grant.lease() {
            report.lapsing.push((app.clone(), error));
            return Err(reason);
        }
        if let Err(error) = self
            .queue
            .give_back_within(
                Budget::at(claim.deadline.give_backs),
                ScopeLock::Skip,
                claim.worker,
                grant.delivery(),
                defer,
                |_| authorized(),
            )
            .await
        {
            report.lapsing.push((app.clone(), error));
        }
        Err(reason)
    }

    /// Pass an app without its lock when a read already shows it has nothing
    /// to give: no row a worker may take under its delivery budget and
    /// barriers, or as many live deliveries as its policy allows.
    ///
    /// These reads take no transaction, so on SQLite they do not queue behind
    /// the database-wide write lock either. They decide nothing: an app they let
    /// through is decided again under its lock.
    async fn passable(
        &self,
        app: &AppId,
        ceiling: i64,
        max_running: i64,
        deadline: Instant,
    ) -> Result<(), ClaimSkipReason> {
        let budget = self.share(deadline);
        let database = &self.queue.database;
        let now = bounded(budget.clone(), self.queue.now())
            .await
            .and_then(|now| now)
            .map_err(ClaimSkipReason::of)?;
        let candidate = bounded(
            budget.clone(),
            Box::pin(crate::scheduling::candidate(
                database,
                app,
                now,
                Claimant::Worker,
                Some(ceiling),
            )),
        )
        .await
        .and_then(|found| found)
        .map_err(ClaimSkipReason::of)?;
        if candidate.is_none() {
            return Err(ClaimSkipReason::NoCandidate);
        }
        let running = bounded(
            budget,
            crate::scheduling::live_advance_count(database, app, now),
        )
        .await
        .and_then(|running| running)
        .map_err(ClaimSkipReason::of)?;
        if running >= max_running {
            return Err(ClaimSkipReason::AtCap);
        }
        Ok(())
    }

    /// One page of the zone's candidate apps after `after`, read without a
    /// transaction under the batch's remaining budget.
    async fn page(&self, claim: &ZoneClaim<'_>, after: Option<&AppId>) -> Result<Vec<AppId>, Error> {
        let budget = self.share(claim.deadline.attempts);
        let now = bounded(budget.clone(), self.queue.now()).await??;
        bounded(
            budget,
            crate::scheduling::claimable_apps_in_zone(
                &self.queue.database,
                now,
                claim.zone,
                after,
                &claim.request.exclude,
                self.options.batch_limit,
            ),
        )
        .await?
    }

    /// One step's budget inside a batch: the queue's transaction timeout, never
    /// past the batch deadline.
    fn share(&self, deadline: Instant) -> Budget {
        Budget::within(self.queue.options.transaction_timeout, deadline)
    }

    /// Extend a delivery's lease for the enrolled worker that holds it.
    ///
    /// # Errors
    /// Refuses a stale delivery, revoked enrollment and failed transactions.
    pub async fn heartbeat_job<F, Fut>(
        &self,
        worker: &WorkerId,
        delivery: &Delivery,
        authorize: F,
    ) -> Result<DeliveryGrant, Error>
    where
        F: Fn() -> Fut,
        Fut: Future<Output = Result<WorkerId, Error>>,
    {
        self.queue
            .heartbeat_authorized(worker, delivery, |_| enrolled(worker, &authorize))
            .await
    }

    /// Settle a delivery with the outcome the journal decided.
    ///
    /// # Errors
    /// Refuses a stale or changed settlement, revoked enrollment and failed
    /// transactions.
    pub async fn settle_job<F, Fut>(
        &self,
        worker: &WorkerId,
        settlement: &JournalSettlement,
        authorize: F,
    ) -> Result<SettlementReceipt, Error>
    where
        F: Fn() -> Fut,
        Fut: Future<Output = Result<WorkerId, Error>>,
    {
        self.queue
            .settle_authorized(
                worker,
                settlement,
                |_| enrolled(worker, &authorize),
                |_| enrolled(worker, &authorize),
            )
            .await
    }

    /// Return a live delivery to the ready queue without settling it, as
    /// `defer` says.
    ///
    /// # Errors
    /// Refuses a stale delivery, revoked enrollment, an invalid deferral and
    /// failed transactions.
    pub async fn give_back_job<F, Fut>(
        &self,
        worker: &WorkerId,
        delivery: &Delivery,
        defer: GiveBack,
        authorize: F,
    ) -> Result<(), Error>
    where
        F: Fn() -> Fut,
        Fut: Future<Output = Result<WorkerId, Error>>,
    {
        self.queue
            .give_back(worker, delivery, defer, |_| enrolled(worker, &authorize))
            .await
    }
}

async fn enrolled<F, Fut>(worker: &WorkerId, authorize: &F) -> Result<WorkerId, Error>
where
    F: Fn() -> Fut,
    Fut: Future<Output = Result<WorkerId, Error>>,
{
    let current = authorize().await?;
    if &current != worker {
        return Err(Error::Denied);
    }
    Ok(current)
}
