//! This service's own lane over the maintenance rows of the queue it owns.
//!
//! The rows it takes are every sweep `maintenance_job` dispatches. It does not
//! take the operation that executes creator code: a worker of the app's zone
//! takes that, and the claim predicate is what keeps the two hosts off each
//! other's rows.
//!
//! The lane holds the payload store, so the sweeps that move creator bytes are
//! its to run. `crate::payloads` is where this process binds that store, and
//! the writer and the deleter reach the engine as arguments rather than as
//! anything the engine holds.
#![allow(
    clippy::future_not_send,
    reason = "the lane stays on the runtime that opened the journal and the queue"
)]

use std::{
    future::Future,
    rc::Rc,
    time::{Duration, Instant},
};

use zeroship_core::{
    app_id::AppId,
    workflow_coordination::WorkerId,
    workflow_jobs::{JobSpec, SettlementReceipt},
};
use zeroship_workflow::{
    service::{
        maintenance::{MaintenanceOptions, MaintenanceOutcome},
        publication::JobPublisher,
    },
    WorkflowServiceError,
};
use zeroship_workflow_manager::{
    maintenance::MaintenanceAuthority, policy::PolicySource, Claimant, Error as ManagerError, Queue,
};

use crate::{payloads::ServicePayloads, runs::RunService};

/// What one visit to an app's queue did.
#[derive(Debug)]
pub enum Swept {
    /// The app holds no maintenance row this lane can take.
    Idle,
    /// The operation committed its outcome and the queue recorded it.
    Settled(Box<SettlementReceipt>),
    /// A fanout page whose predecessor has not finished. Nothing was committed,
    /// and there is no outcome to record.
    Deferred,
}

/// Which side of the lane refused.
///
/// The two are kept apart rather than flattened: a queue refusal is about
/// authority or the row's state, and a journal refusal is about the operation.
/// An operator acts on them differently.
#[derive(Debug)]
pub enum SweepError {
    Queue(ManagerError),
    Journal(WorkflowServiceError),
    /// The turn's deadline cut the visit off before either side answered. It is
    /// the host's own bound rather than a refusal, so no side is named; a row
    /// the visit had leased keeps its lease until it lapses. The scan position
    /// advances before the visit, so the rest of this pass skips that app and
    /// the next PASS reaches it again - unlike the apps a turn never reached,
    /// which its successor takes. Only a turn reports this;
    /// [`MaintenanceLane::sweep`] carries no deadline of its own.
    Deadline,
}

/// Claims and runs this service's maintenance rows, one app at a time.
#[derive(Debug)]
pub struct MaintenanceLane {
    queue: Queue,
    runs: Rc<RunService>,
    policies: Rc<dyn PolicySource>,
    identity: WorkerId,
    payloads: ServicePayloads,
    options: MaintenanceOptions,
}

impl MaintenanceLane {
    /// `identity` names this process on every row the lane leases, and
    /// `payloads` is the store the byte-moving sweeps write and delete through.
    ///
    /// # Errors
    /// Refuses an invalid page or batch bound on any maintenance operation.
    pub fn new(
        queue: Queue,
        runs: Rc<RunService>,
        policies: Rc<dyn PolicySource>,
        identity: WorkerId,
        payloads: ServicePayloads,
        options: MaintenanceOptions,
    ) -> Result<Self, WorkflowServiceError> {
        options.validate()?;
        Ok(Self {
            queue,
            runs,
            policies,
            identity,
            payloads,
            options,
        })
    }

    /// The store this lane stages run inputs into and collects objects from, so
    /// a caller that composed the lane can reach the same objects its sweeps do.
    #[must_use]
    pub const fn payloads(&self) -> &ServicePayloads {
        &self.payloads
    }

    #[must_use]
    pub const fn identity(&self) -> &WorkerId {
        &self.identity
    }

    /// Take one maintenance row of `app`'s queue, run it and record what it
    /// committed.
    ///
    /// # Errors
    /// Reports a refused claim or settlement, an unavailable policy source, a
    /// journal this app may not bind, and whatever the named operation refuses.
    pub async fn sweep(&self, app: &AppId) -> Result<Swept, SweepError> {
        let engine = self
            .runs
            .app(self.policies.as_ref(), app)
            .await
            .map_err(SweepError::Journal)?;
        let authority = MaintenanceAuthority::new(app.clone(), self.identity.clone());
        let ceiling = self
            .policies
            .observe(app)
            .await
            .map(|observed| observed.policy().max_delivery_attempts);
        let Some(grant) = authority
            .claim(&self.queue, ceiling)
            .await
            .map_err(SweepError::Queue)?
        else {
            return Ok(Swept::Idle);
        };
        let publisher = LanePublisher::new(&self.queue, app.clone());
        let receipt = match engine
            .maintenance_job(
                &grant,
                &publisher,
                &self.payloads,
                &self.payloads,
                self.options,
            )
            .await
            .map_err(SweepError::Journal)?
        {
            MaintenanceOutcome::Settled(receipt) => *receipt,
            MaintenanceOutcome::Deferred => return Ok(Swept::Deferred),
            // The claim admits exactly the kinds this dispatch has an arm for,
            // so reaching here means the two disagree about one of them.
            MaintenanceOutcome::Unclaimed => {
                return Err(SweepError::Journal(WorkflowServiceError::Internal(
                    "workflow maintenance claimed an operation its dispatch does not run".into(),
                )))
            }
        };
        let settlement = receipt
            .settlement(&grant)
            .map_err(|refusal| SweepError::Journal(refusal.into()))?;
        authority
            .settle(&self.queue, &settlement)
            .await
            .map(|receipt| Swept::Settled(Box::new(receipt)))
            .map_err(SweepError::Queue)
    }
}

/// The bounds one turn of the lane runs under, from the host's configuration.
///
/// They are the manager driver's own lane bounds, because this lane takes its
/// turn beside that driver's on one runtime: a page nobody bounds and a turn
/// nobody times would spend the process's only thread on one busy app.
#[derive(Clone, Copy, Debug)]
pub struct LaneOptions {
    /// Maximum claimable rows a turn's enumeration fetches, which bounds the
    /// apps it visits: one app's rows are adjacent in that scan and collapse to
    /// one visit, so a turn visits at most this many apps and often fewer.
    pub page_limit: u32,
    /// Shared deadline for a turn's enumeration and its entire page.
    pub lane_timeout: Duration,
}

impl LaneOptions {
    /// # Errors
    /// Rejects an empty page, a page wider than one storage read, and a
    /// deadline that is zero or beyond the portable clock's range.
    pub fn validate(self) -> Result<(), WorkflowServiceError> {
        if self.page_limit == 0
            || i64::from(self.page_limit) > zeroship_data_orm::sql::MAX_ROW_LIMIT
            || self.lane_timeout.is_zero()
            || Instant::now().checked_add(self.lane_timeout).is_none()
        {
            return Err(WorkflowServiceError::InvalidRequest(
                "workflow maintenance turn bounds are invalid".into(),
            ));
        }
        Ok(())
    }
}

/// One app's visit that did not finish, and why.
#[derive(Debug)]
pub struct SweepFailure {
    pub app: AppId,
    pub error: SweepError,
}

/// What one turn did: what it swept, what it refused, and whether the pass
/// finished or was cut off.
#[derive(Debug, Default)]
pub struct SweepReport {
    /// Apps this turn visited.
    pub visited: usize,
    /// Visits that ran a row and recorded its settlement.
    pub settled: usize,
    /// Visits that claimed a fanout page whose predecessor has not finished.
    pub deferred: usize,
    /// Visits that found no row this lane can take, because another claimant
    /// took it first or because the claim decided it is not deliverable yet.
    pub idle: usize,
    pub failures: Vec<SweepFailure>,
    /// The enumeration itself failed, so the turn visited nothing.
    pub scan_error: Option<ManagerError>,
    pub timed_out: bool,
    /// Listed apps left for a later turn when the deadline expired.
    pub unvisited: usize,
    /// The pass reached the upper bound it started against, so the next turn
    /// begins a new one at the lowest app id again.
    pub sweep_complete: bool,
}

/// Gives the lane bounded turns over the apps whose queue holds a row it takes.
///
/// It owns the bounds and the scan position; the lane owns the work, exactly as
/// the manager's `Driver` owns the cursors and pages of the operations it
/// drives. A turn enumerates apps, and the claim inside each visit is what
/// decides whether the app really has a row for this lane: the enumeration reads
/// no app lock, so it can list an app whose row another claimant takes first.
#[derive(Debug)]
pub struct MaintenanceDriver {
    lane: MaintenanceLane,
    options: LaneOptions,
    cursor: Cursor,
}

/// A disposable scan position, not authority and not durable work. `upper` is
/// the greatest app id the pass started against, so a pass over a queue that
/// keeps gaining apps still terminates.
#[derive(Debug, Default)]
struct Cursor {
    after: Option<String>,
    upper: Option<String>,
}

impl MaintenanceDriver {
    /// # Errors
    /// Rejects invalid turn bounds.
    pub fn new(lane: MaintenanceLane, options: LaneOptions) -> Result<Self, WorkflowServiceError> {
        options.validate()?;
        Ok(Self {
            lane,
            options,
            cursor: Cursor::default(),
        })
    }

    /// The lane this driver gives turns to, so a caller that composed the driver
    /// can read the identity every row it leases carries.
    #[must_use]
    pub const fn lane(&self) -> &MaintenanceLane {
        &self.lane
    }

    /// Visit one bounded page of the apps holding rows this lane takes.
    ///
    /// The deadline covers the enumeration and the whole page together. A visit
    /// that refuses or is cut off is recorded and the turn goes on to the next
    /// app: one app's failure never costs another its turn, and nothing here
    /// retries within the turn. Dropping this future abandons the rest of the
    /// page without deleting any durable work.
    pub async fn tick(&mut self) -> SweepReport {
        let deadline = Deadline(Instant::now() + self.options.lane_timeout);
        let mut report = SweepReport::default();
        let (page, fetched) = match self.page(deadline).await {
            Ok(page) => page,
            Err(error) => {
                report.scan_error = Some(error);
                report.timed_out = deadline.expired();
                return report;
            }
        };
        for app in &page {
            if deadline.expired() {
                report.timed_out = true;
                break;
            }
            self.cursor.after = Some(app.as_str().to_owned());
            report.visited += 1;
            match deadline.run(self.lane.sweep(app)).await {
                Some(Ok(Swept::Settled(_))) => report.settled += 1,
                Some(Ok(Swept::Deferred)) => report.deferred += 1,
                Some(Ok(Swept::Idle)) => report.idle += 1,
                Some(Err(error)) => report.failures.push(SweepFailure {
                    app: app.clone(),
                    error,
                }),
                None => {
                    report.timed_out = true;
                    report.failures.push(SweepFailure {
                        app: app.clone(),
                        error: SweepError::Deadline,
                    });
                }
            }
        }
        report.unvisited = page.len() - report.visited;
        if !report.timed_out
            && (fetched < self.options.page_limit as usize
                || self.cursor.after == self.cursor.upper)
        {
            self.cursor = Cursor::default();
            report.sweep_complete = true;
        }
        report
    }

    /// The apps this turn visits and the rows the scan fetched to name them,
    /// under the deadline the turn shares.
    ///
    /// The upper bound is read once per pass and held across turns, so a queue
    /// that gains apps while a pass runs does not extend it.
    async fn page(&mut self, deadline: Deadline) -> Result<(Vec<AppId>, usize), ManagerError> {
        if self.cursor.upper.is_none() {
            let (highest, _) = deadline
                .run(
                    self.lane
                        .queue
                        .claimable_apps(Claimant::Maintenance, None, None, true, 1),
                )
                .await
                .ok_or(ManagerError::Timeout)??;
            self.cursor.upper = highest.first().map(|app| app.as_str().to_owned());
        }
        let Some(upper) = self.cursor.upper.as_deref() else {
            return Ok((Vec::new(), 0));
        };
        deadline
            .run(self.lane.queue.claimable_apps(
                Claimant::Maintenance,
                self.cursor.after.as_deref(),
                Some(upper),
                false,
                self.options.page_limit,
            ))
            .await
            .ok_or(ManagerError::Timeout)?
    }
}

#[derive(Clone, Copy)]
struct Deadline(Instant);

impl Deadline {
    fn expired(self) -> bool {
        Instant::now() >= self.0
    }

    /// What `future` answered, or `None` when the deadline cut it off.
    ///
    /// An answer that lands as the deadline passes is kept rather than
    /// discarded: what it reports is already committed, and the turn records the
    /// expiry itself before it visits anything else.
    async fn run<T>(self, future: impl Future<Output = T>) -> Option<T> {
        if self.expired() {
            return None;
        }
        compio::time::timeout(self.0.saturating_duration_since(Instant::now()), future)
            .await
            .ok()
    }
}

/// Publishes a sweep's successors straight into the queue this process owns.
///
/// One journal serves every app the service holds, and the `app_id` columns
/// inside it are the whole of what tells them apart. So a publication carrying
/// another app's id is a tenant boundary crossed, and the boundary has to be
/// asserted wherever one is composed rather than wherever one is expected.
///
/// `Queue::submit` asserts nothing of the kind, and is right not to: it is the
/// manager-origin path, and a manager-origin caller has no identity to compare
/// a job against. No worker path publishes a job at all. This seam has an app,
/// so it is the only place the comparison can be made.
#[derive(Debug)]
pub struct LanePublisher<'a> {
    queue: &'a Queue,
    app: AppId,
}

impl<'a> LanePublisher<'a> {
    /// Publish into `queue` as `app`, and as nothing else.
    #[must_use]
    pub const fn new(queue: &'a Queue, app: AppId) -> Self {
        Self { queue, app }
    }
}

impl JobPublisher for LanePublisher<'_> {
    fn app_id(&self) -> &AppId {
        &self.app
    }

    async fn submit(&self, job: &JobSpec) -> Result<JobSpec, WorkflowServiceError> {
        // The tenant boundary, not a defence against a caller that can reach
        // it: the sweep this serves publishes from its own app's journal rows,
        // so nothing today composes a foreign job here.
        if job.app_id != self.app {
            return Err(WorkflowServiceError::PermissionDenied);
        }
        self.queue.submit(job).await.map_err(|error| {
            WorkflowServiceError::Unavailable(format!(
                "workflow maintenance publication was refused: {error:?}"
            ))
        })
    }
}
