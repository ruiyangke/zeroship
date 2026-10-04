//! Pull claimable deliveries from the worker's execution zone into bounded slots.
//!
//! ONE CLAIMER PER HOST, NOT ONE PER SLOT. The claimer asks for exactly as many
//! deliveries as it has free slots, in one batch, and hands each delivery to a
//! slot that prepares the delivery's app and runs it. The claim continues from
//! the cursor the previous reply returned, so this worker visits the zone's
//! apps in turn and its own slots never compete for the same row.

#![expect(
    clippy::future_not_send,
    reason = "consumer resources stay on their owning compio thread"
)]

use crate::{
    delivery::{
        bounded, Claimed, ClaimedBatch, DeliveryOptions, DeliverySlot, JobTransport, Unstarted,
    },
    prepared::{CreatorFactory, Prepared, PreparedApps},
};
use futures::{
    future::{Either, LocalBoxFuture, Shared},
    stream::FuturesUnordered,
    FutureExt, StreamExt,
};
use std::{
    cell::{Cell, RefCell},
    collections::BTreeMap,
    future::Future,
    num::{NonZeroU32, NonZeroU64},
    rc::Rc,
    time::{Duration, Instant},
};
use zeroship_core::{
    app_id::AppId,
    workflow_coordination::WorkerId,
    workflow_jobs::{ClaimJobs, JobLease},
};
use zeroship_workflow::WorkflowServiceError;

/// Host bounds, independent of the manager's queue and the journal.
#[derive(Debug, Clone, Copy)]
pub struct ConsumerOptions {
    /// Deliveries executing at once.
    pub slots: usize,
    /// Delay before claiming again once a claim reached the end of the zone
    /// without filling the free slots it asked for.
    pub idle_poll: Duration,
    /// Delay before claiming again after a refused claim, and the first expiry
    /// of an app this host failed to prepare.
    pub error_backoff: Duration,
    /// How long the deliveries running when a stop arrives get to finish and
    /// settle before they are cancelled, counted from the stop. Zero cancels
    /// them at the stop.
    pub drain: Duration,
    pub delivery: DeliveryOptions,
}

impl ConsumerOptions {
    fn validate(self) -> Result<(), WorkflowServiceError> {
        self.delivery.validate()?;
        if self.slots == 0
            || u32::try_from(self.slots).is_err()
            || u64::try_from(self.delivery.operation_timeout.as_millis()).is_err()
            || Instant::now().checked_add(self.drain).is_none()
            || [self.idle_poll, self.error_backoff, skip_ceiling(self.error_backoff)]
                .into_iter()
                .any(|delay| delay.is_zero() || Instant::now().checked_add(delay).is_none())
        {
            return Err(WorkflowServiceError::InvalidRequest(
                "invalid workflow consumer limits".into(),
            ));
        }
        Ok(())
    }
}

/// How many times a repeated preparation failure doubles an app's expiry.
const SKIP_DOUBLINGS: u32 = 6;

/// The longest an app stays on the skip list after one failure.
const fn skip_ceiling(backoff: Duration) -> Duration {
    backoff.saturating_mul(1 << SKIP_DOUBLINGS)
}

/// Apps this host failed to prepare, withheld from its own claims until each
/// entry expires.
///
/// LOCAL AND UNWRITTEN. Nothing is recorded server-side: the list only narrows
/// what this worker asks for, and it dies with the process. The row itself went
/// back to the queue with its own back-off when the preparation failed.
#[derive(Debug, Default)]
struct SkipList {
    entries: BTreeMap<AppId, Skip>,
}

#[derive(Debug, Clone, Copy)]
struct Skip {
    failures: u32,
    until: Instant,
}

impl SkipList {
    /// Record a failed preparation. The expiry starts at `backoff` and doubles
    /// with each consecutive failure, up to its ceiling.
    fn failed(&mut self, app: &AppId, now: Instant, backoff: Duration) {
        let failures = self
            .entries
            .get(app)
            .map_or(1, |skip| skip.failures.saturating_add(1));
        let expiry = backoff.saturating_mul(1 << (failures - 1).min(SKIP_DOUBLINGS));
        let until = now
            .checked_add(expiry)
            .or_else(|| now.checked_add(skip_ceiling(backoff)))
            .unwrap_or(now);
        self.entries.insert(app.clone(), Skip { failures, until });
    }

    /// A successful preparation ends the run of consecutive failures.
    fn prepared(&mut self, app: &AppId) {
        self.entries.remove(app);
    }

    /// The apps a claim made now excludes.
    ///
    /// An entry that has been expired for a whole ceiling is forgotten, so an
    /// app that fails again after a quiet period starts its doubling over.
    fn excluded(&mut self, now: Instant, backoff: Duration) -> Vec<AppId> {
        let ceiling = skip_ceiling(backoff);
        self.entries
            .retain(|_, skip| skip.until.checked_add(ceiling).is_none_or(|end| now < end));
        let mut live: Vec<_> = self
            .entries
            .iter()
            .filter(|(_, skip)| skip.until > now)
            .collect();
        live.sort_by_key(|(_, skip)| std::cmp::Reverse(skip.until));
        live.into_iter()
            // The wire bound: the service refuses a longer list, so the apps whose
            // entries expire last are the ones sent.
            .take(ClaimJobs::MAX_EXCLUDE)
            .map(|(app, _)| app.clone())
            .collect()
    }
}

/// One execution slot: the delivery it runs and the prepared app it holds.
///
/// THE SLOT HOLDS THE PREPARED APP, not the future running in it. A run whose
/// future is dropped leaves its cancelled execution in the delivery slot until
/// `drain` joins it, and the app's residency has to outlive that join.
struct Slot<T: JobTransport> {
    delivery: Option<DeliverySlot<T>>,
    held: Option<Rc<Prepared<T::Journal>>>,
}

impl<T: JobTransport> Slot<T> {
    async fn drain(&mut self) {
        if let Some(delivery) = &mut self.delivery {
            delivery.drain_interrupted().await;
        }
        self.held = None;
    }
}

type Stop<'a> = Shared<LocalBoxFuture<'a, ()>>;
type Running<'s, T> = FuturesUnordered<LocalBoxFuture<'s, &'s mut Slot<T>>>;

/// A bounded job consumer pulling from the worker's execution zone.
pub struct JobConsumer<T: JobTransport, F: CreatorFactory<Journal = T::Journal>> {
    transport: Rc<T>,
    worker: WorkerId,
    prepared: Rc<PreparedApps<F>>,
    options: ConsumerOptions,
    slots: Vec<Slot<T>>,
    cursor: Option<AppId>,
    skipped: RefCell<SkipList>,
}

impl<T: JobTransport, F: CreatorFactory<Journal = T::Journal>> std::fmt::Debug
    for JobConsumer<T, F>
{
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("JobConsumer")
            .field("worker", &self.worker.as_str())
            .field("options", &self.options)
            .finish_non_exhaustive()
    }
}

impl<T: JobTransport, F: CreatorFactory<Journal = T::Journal>> JobConsumer<T, F> {
    /// # Errors
    /// Refuses invalid capacity and time bounds.
    pub fn new(
        transport: Rc<T>,
        worker: WorkerId,
        prepared: Rc<PreparedApps<F>>,
        options: ConsumerOptions,
    ) -> Result<Self, WorkflowServiceError> {
        options.validate()?;
        Ok(Self {
            transport,
            worker,
            prepared,
            slots: (0..options.slots)
                .map(|_| Slot {
                    delivery: None,
                    held: None,
                })
                .collect(),
            options,
            cursor: None,
            skipped: RefCell::new(SkipList::default()),
        })
    }

    /// Claim and run deliveries until `shutdown`; then claim nothing more, let
    /// the deliveries already running finish and settle until ONE deadline,
    /// [`ConsumerOptions::drain`] after the stop arrived, and cancel and join
    /// whatever is still running at it. A claim pending when the stop arrives
    /// runs to its reply, and every delivery it brings goes back unstarted,
    /// beside the running deliveries: they are driven the whole time, and
    /// neither the claim nor its give-backs move the deadline.
    ///
    /// Dropping this future cancels work but retains occupied slots. Call
    /// `drain` before discarding the consumer; calling this again also drains
    /// first.
    pub async fn run_until(&mut self, shutdown: impl Future<Output = ()>) {
        self.drain().await;
        // THE STOP INSTANT. Every wait below watches the stop, so the instant
        // it resolves at is the instant it arrived, and the grace is counted
        // from there.
        let stopped_at = Rc::new(Cell::new(None));
        let shutdown: Stop<'_> = {
            let stopped_at = stopped_at.clone();
            async move {
                shutdown.await;
                stopped_at.set(Some(Instant::now()));
            }
            .boxed_local()
            .shared()
        };
        let Self {
            transport,
            worker,
            prepared,
            options,
            slots,
            cursor,
            skipped,
        } = self;
        let options = *options;
        let (transport, worker, prepared, skipped) = (&*transport, &*worker, &**prepared, &*skipped);
        let mut free: Vec<&mut Slot<T>> = slots.iter_mut().collect();
        let mut running: Running<'_, T> = FuturesUnordered::new();
        // What the stop leaves to finish beside the grace: a claim pending at
        // the stop, run on to its reply, and the give-back of what a reply
        // delivered after the stop.
        let mut tail: Option<LocalBoxFuture<'_, ()>> = None;
        loop {
            if shutdown.clone().now_or_never().is_some() {
                break;
            }
            // Apps absent from the version feed leave the cache on every
            // claim cycle, whether or not this cycle delivers anything.
            prepared.prune();
            if free.is_empty() {
                match stopped(shutdown.clone(), running.next()).await {
                    None => break,
                    Some(finished) => free.extend(finished),
                }
                continue;
            }
            let Some(request) = claim_request(free.len(), cursor.as_ref(), skipped, options)
            else {
                failed(&WorkflowServiceError::InvalidRequest(
                    "unrepresentable workflow claim".into(),
                ));
                break;
            };
            let asked = usize::try_from(request.max.get()).unwrap_or(usize::MAX);
            // A PENDING CLAIM IS NOT CANCELLED BY A STOP. The service commits
            // each delivery before it replies, so dropping the exchange would
            // strand every one of them until its lease lapsed. The stop is
            // watched beside the claim; a claim the stop finds pending becomes
            // the tail, which runs on to the reply its own wait bounds and gives
            // back what it delivers.
            let mut claiming = claim(transport.as_ref(), request, options).boxed_local();
            let claimed = {
                let pending = std::pin::pin!(alongside(&mut claiming, &mut running, &mut free));
                match futures::future::select(pending, shutdown.clone()).await {
                    Either::Left((claimed, _)) => Some(claimed),
                    Either::Right(((), _)) => None,
                }
            };
            let Some(claimed) = claimed else {
                tail = Some(give_back_claimed(transport.as_ref(), claiming, options).boxed_local());
                break;
            };
            if shutdown.clone().now_or_never().is_some() {
                if let Ok(batch) = claimed {
                    tail = Some(
                        give_back_all(transport.as_ref(), batch.deliveries, options).boxed_local(),
                    );
                }
                break;
            }
            let batch = match claimed {
                Ok(batch) => batch,
                Err(error) => {
                    failed(&error);
                    let pause = compio::time::sleep(options.error_backoff);
                    if stopped(shutdown.clone(), alongside(pause, &mut running, &mut free))
                        .await
                        .is_none()
                    {
                        break;
                    }
                    continue;
                }
            };
            *cursor = batch.after;
            // The zone ran out before this claim filled what it asked for. An
            // empty reply that did not reach the end is a claim that ran out of
            // time, and the next one continues from its cursor at once.
            let exhausted = batch.lap_complete && batch.deliveries.len() < asked;
            for claimed in batch.deliveries {
                if claimed.lease.delivery().worker_id != *worker {
                    failed(&WorkflowServiceError::PermissionDenied);
                    continue;
                }
                let Some(slot) = free.pop() else {
                    failed(&WorkflowServiceError::InvalidResponse(
                        "workflow claim returned more deliveries than it asked for".into(),
                    ));
                    continue;
                };
                running.push(
                    execute(slot, transport, prepared, skipped, options, claimed).boxed_local(),
                );
            }
            if exhausted && stopped(shutdown.clone(), idle(options.idle_poll, &mut running, &mut free))
                .await
                .is_none()
            {
                break;
            }
        }
        // THE GRACE. Nothing more is claimed; what is running finishes and
        // settles until the deadline, so a stop costs no work that could still
        // complete. The deadline is counted from the stop, not from here, and
        // the tail runs beside the grace while the running deliveries are
        // driven: a claim in flight at the stop spends none of their time.
        let stopped = stopped_at.get().unwrap_or_else(Instant::now);
        let grace = async {
            let finishing = async { while running.next().await.is_some() {} };
            let remaining = options.drain.saturating_sub(stopped.elapsed());
            if compio::time::timeout(remaining, finishing).await.is_err() && !running.is_empty() {
                if !options.drain.is_zero() {
                    tracing::warn!(
                        drain_ms = u64::try_from(options.drain.as_millis()).unwrap_or(u64::MAX),
                        "workflow deliveries still running at the drain bound are cancelled"
                    );
                }
                // Clearing the running set cancels each execution still in it,
                // in place, at the deadline rather than when the tail ends; its
                // slot keeps the cancelled execution and the app it holds for
                // the drain.
                running.clear();
            }
        };
        futures::future::join(
            async {
                if let Some(tail) = tail {
                    tail.await;
                }
            },
            grace,
        )
        .await;
        // The grace left nothing running: what it did not finish it cancelled.
        // Releasing the set and the free list returns the slots to the drain.
        drop(running);
        drop(free);
        self.drain().await;
    }

    /// Join interrupted execution before reclaiming any local capacity.
    pub async fn drain(&mut self) {
        futures::future::join_all(self.slots.iter_mut().map(Slot::drain)).await;
    }
}

/// The free slots up to the protocol's bound, the previous reply's cursor and
/// the live skip list.
fn claim_request(
    free: usize,
    cursor: Option<&AppId>,
    skipped: &RefCell<SkipList>,
    options: ConsumerOptions,
) -> Option<ClaimJobs> {
    Some(ClaimJobs {
        max: NonZeroU32::new(
            u32::try_from(free)
                .unwrap_or(u32::MAX)
                .min(ClaimJobs::MAX_DELIVERIES),
        )?,
        // The host's claim bound. The service stops its work for the batch
        // inside this wait and the transport waits exactly this long for the
        // reply, so the two ends agree without sharing a setting.
        wait_ms: NonZeroU64::new(
            u64::try_from(options.delivery.operation_timeout.as_millis()).ok()?,
        )?,
        after: cursor.cloned(),
        exclude: skipped
            .borrow_mut()
            .excluded(Instant::now(), options.error_backoff),
    })
}

/// One claim, bounded by the wait it states and one operation bound after it:
/// the transport's exchange lasts exactly `wait_ms`, and a delivery that
/// arrived unusable is given back inside the operation bound that follows.
async fn claim<T: JobTransport>(
    transport: &T,
    request: ClaimJobs,
    options: ConsumerOptions,
) -> Result<ClaimedBatch<T::Lease>, WorkflowServiceError> {
    let wait = Duration::from_millis(request.wait_ms.get());
    bounded(
        wait.saturating_add(options.delivery.operation_timeout),
        transport.claim(&request),
    )
    .await
}

/// The tail of a claim a stop found pending: it runs on to its reply, and every
/// delivery the reply brings goes back unstarted.
async fn give_back_claimed<T: JobTransport>(
    transport: &T,
    claiming: LocalBoxFuture<'_, Result<ClaimedBatch<T::Lease>, WorkflowServiceError>>,
    options: ConsumerOptions,
) {
    if let Ok(batch) = claiming.await {
        give_back_all(transport, batch.deliveries, options).await;
    }
}

/// Give back, unstarted, every delivery a claim answered after the host
/// stopped: each row is claimable at once and counts nothing, and a journal
/// task accepted for it is released with it.
async fn give_back_all<T: JobTransport>(
    transport: &T,
    deliveries: Vec<Claimed<T::Lease>>,
    options: ConsumerOptions,
) {
    let returns = deliveries.iter().map(|claimed| {
        bounded(
            options.delivery.operation_timeout,
            transport.give_back(claimed, Unstarted::Stopped),
        )
    });
    for result in futures::future::join_all(returns).await {
        if let Err(error) = result {
            failed(&error);
        }
    }
}

/// Prepare the delivery's app under its remaining lease, then run it.
async fn execute<'s, T, F>(
    slot: &'s mut Slot<T>,
    transport: &Rc<T>,
    prepared: &PreparedApps<F>,
    skipped: &RefCell<SkipList>,
    options: ConsumerOptions,
    claimed: Claimed<T::Lease>,
) -> &'s mut Slot<T>
where
    T: JobTransport,
    F: CreatorFactory<Journal = T::Journal>,
{
    let lease = &claimed.lease;
    let Some(remaining) = lease
        .remaining()
        .zip(lease.attempt_remaining())
        .map(|(lease, attempt)| lease.min(attempt))
    else {
        failed(&WorkflowServiceError::Timeout);
        return slot;
    };
    let app = lease.delivery().job.app_id.clone();
    let entry = match prepared.get_or_prepare(&app, remaining).await {
        Ok(entry) => {
            skipped.borrow_mut().prepared(&app);
            entry
        }
        Err(error) => {
            failed(&error);
            skipped
                .borrow_mut()
                .failed(&app, Instant::now(), options.error_backoff);
            if let Err(error) = bounded(
                options.delivery.operation_timeout,
                transport.give_back(&claimed, Unstarted::Unprepared),
            )
            .await
            {
                failed(&error);
            }
            return slot;
        }
    };
    slot.held = Some(entry.clone());
    let delivery = match DeliverySlot::new(transport.clone(), entry.executor(), options.delivery)
    {
        Ok(delivery) => slot.delivery.insert(delivery),
        Err(error) => {
            failed(&error);
            slot.held = None;
            return slot;
        }
    };
    if let Err(error) = delivery.run(entry.journal(), claimed).await {
        failed(&error);
    }
    slot.held = None;
    slot
}

/// Drive `work` while the running executions progress, returning each slot an
/// execution frees to the free list.
async fn alongside<'s, T: JobTransport, O>(
    work: impl Future<Output = O>,
    running: &mut Running<'s, T>,
    free: &mut Vec<&'s mut Slot<T>>,
) -> O {
    let mut work = std::pin::pin!(work);
    loop {
        if running.is_empty() {
            return work.await;
        }
        match futures::future::select(work.as_mut(), running.next()).await {
            Either::Left((output, _)) => return output,
            Either::Right((finished, _)) => free.extend(finished),
        }
    }
}

/// Wait out the idle interval, or less: a slot that frees while it runs claims
/// at once, because the settlement that freed it may have published the run's
/// next job.
async fn idle<'s, T: JobTransport>(
    delay: Duration,
    running: &mut Running<'s, T>,
    free: &mut Vec<&'s mut Slot<T>>,
) {
    let sleep = std::pin::pin!(compio::time::sleep(delay));
    if running.is_empty() {
        return sleep.await;
    }
    if let Either::Right((finished, _)) = futures::future::select(sleep, running.next()).await {
        free.extend(finished);
    }
}

/// Record a consumption failure.
///
/// The REASON is logged beside the code. Several distinct refusals share one
/// code, so a line carrying only the code cannot tell an operator which of
/// them stopped this worker from consuming.
fn failed(error: &WorkflowServiceError) {
    tracing::warn!(code = error.code(), reason = %error, "workflow job consumption failed");
}

async fn stopped<T>(stop: Stop<'_>, work: impl Future<Output = T>) -> Option<T> {
    match futures::future::select(stop, work.boxed_local()).await {
        Either::Left(((), work)) => {
            drop(work);
            None
        }
        Either::Right((value, _)) => Some(value),
    }
}

#[cfg(test)]
mod tests;
