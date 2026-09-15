//! Bounded manager maintenance, independent of workers and creator storage.
#![expect(
    clippy::future_not_send,
    reason = "driver operations share their queue's owning compio runtime"
)]

use crate::{
    capacity::{self, Capacity, Contract, IntentState},
    coordinator::Coordinator,
    eligibility::ZoneId,
    lifecycle::AppLifecycle,
    models::{
        capacity_demands, capacity_intents, capacity_targets, jobs, recovery_duties,
        schema::{deployment_holds, schedules},
    },
    recovery::{self, DutyKind, Recovery},
    retention,
    scheduling::{self, Due as Scheduled, Scheduler},
    Error, Queue,
};
use std::{
    collections::BTreeSet,
    future::Future,
    rc::Rc,
    time::{Duration, Instant},
};
use zeroship_core::{app_id::AppId, workflow_jobs::DeploymentId, workflow_schedules::ScheduleId};
use zeroship_data_orm::orm::{sql_types::Text, Field, Filter, FilterableColumn, FromRow};

#[derive(Clone, Copy, Debug)]
pub struct Options {
    pub scheduling: scheduling::Options,
    /// Duty cadence and the closing lane's idleness, timeout and backoff.
    pub recovery: recovery::Options,
    pub capacity: capacity::Options,
    /// Maximum candidate records visited in each lane's turn.
    pub page_limit: u32,
    /// Shared deadline for a lane's scans and entire candidate page.
    pub lane_timeout: Duration,
    /// How long a confirmed queue hold stays held before the retention lane
    /// may release it. It must exceed the queue's transaction timeout, the
    /// budget within which an acquirer commits the dependency it confirmed.
    pub hold_grace: Duration,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            scheduling: scheduling::Options::default(),
            recovery: recovery::Options::default(),
            capacity: capacity::Options::default(),
            page_limit: 128,
            lane_timeout: Duration::from_secs(10),
            hold_grace: Duration::from_secs(60),
        }
    }
}

impl Options {
    /// Validate host configuration without opening storage or starting tasks.
    ///
    /// # Errors
    /// Rejects empty bounds and values outside the portable clock and row ranges.
    pub fn validate(&self) -> Result<(), Error> {
        if self.page_limit == 0
            || i64::from(self.page_limit) > zeroship_data_orm::sql::MAX_ROW_LIMIT
            || self.lane_timeout.is_zero()
            || Instant::now().checked_add(self.lane_timeout).is_none()
            || self.hold_grace.is_zero()
            || i64::try_from(self.hold_grace.as_millis()).is_err()
            || self.scheduling.max_schedules == 0
            || self.scheduling.max_backfill == 0
            || i64::try_from(self.scheduling.max_backfill).is_err()
            || self.scheduling.min_interval_ms <= 0
            || self.scheduling.page_size == 0
            || i64::from(self.scheduling.page_size) > zeroship_data_orm::sql::MAX_ROW_LIMIT
        {
            return Err(Error::Invalid);
        }
        self.recovery.validate()?;
        self.capacity.validate()
    }
}

/// A storage identity and closed error; no deployment contents or credentials.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CandidateFailure {
    pub id: String,
    pub error: Error,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct LaneReport {
    pub visited: usize,
    pub completed: usize,
    pub failures: Vec<CandidateFailure>,
    pub scan_error: Option<Error>,
    pub timed_out: bool,
    /// Fetched candidates left for a later turn when the lane deadline expired.
    pub unvisited: usize,
    pub sweep_complete: bool,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct TickReport {
    pub scheduling: LaneReport,
    pub reconciliation: LaneReport,
    pub collection: LaneReport,
    pub retention: LaneReport,
    pub closing: LaneReport,
    /// Apps with claimable work, placed on free eligible capacity or recorded
    /// as unplaced demand.
    pub placement: LaneReport,
    /// Apps already recorded as unplaced, revisited until placed or idle.
    pub unplaced: LaneReport,
    /// Per-zone capacity targets applied through the injected provider.
    pub capacity: LaneReport,
}

impl TickReport {
    /// Every lane's report with its name, in a stable order for host logging.
    #[must_use]
    pub fn lanes(self) -> [(&'static str, LaneReport); 8] {
        [
            ("scheduling", self.scheduling),
            ("reconciliation", self.reconciliation),
            ("collection", self.collection),
            ("retention", self.retention),
            ("closing", self.closing),
            ("placement", self.placement),
            ("unplaced", self.unplaced),
            ("capacity", self.capacity),
        ]
    }
}

/// A host calls `tick` and owns cadence, cancellation and shutdown.
///
/// Cursors are disposable scan positions, not authority or durable work. Failed
/// candidates advance within a captured identity range and retry on another sweep.
/// Existing operation transactions own publication, retention and commit fences.
/// No placement or worker is required to generate due jobs.
///
/// The closing lane visits closing attempts and open scopes that are archived
/// or idle past their backoff. It abandons the responsibility of an app whose
/// deletion `lifecycle` reports instead of closing it; a page whose deletion
/// state cannot be read visits nothing.
///
/// The placement lanes key on claimable jobs. The recovery lanes turn every due
/// duty into a pending job before them, and a closing scope's Close job is a
/// job, so every scope that still holds responsibility gets an owner when its
/// work falls due. Apps whose demand free capacity cannot absorb become the
/// durable input of the capacity lane.
#[derive(Debug)]
pub struct Driver {
    queue: Queue,
    scheduler: Scheduler,
    recovery: Recovery,
    lifecycle: Rc<dyn AppLifecycle>,
    capacity: Capacity,
    options: Options,
    cursors: [Cursor; 8],
    next_lane: usize,
}

#[derive(Debug, Default)]
struct Cursor {
    after: Option<String>,
    upper: Option<String>,
}

impl Driver {
    /// Construct every maintenance operation over the coordinator's platform
    /// queue. The coordinator carries the eligibility source placement reads;
    /// the contract names the injected capacity provider. `lifecycle` reports
    /// Control's terminal deletions to the closing lane.
    ///
    /// # Errors
    /// Rejects invalid maintenance options or incompatible native metadata.
    pub fn new(
        coordinator: Coordinator,
        options: Options,
        lifecycle: Rc<dyn AppLifecycle>,
        contract: Contract,
    ) -> Result<Self, Error> {
        options.validate()?;
        let queue = coordinator.queue().clone();
        queue.database.entity::<schedules::Entity>()?;
        queue.database.entity::<recovery_duties::Entity>()?;
        queue.database.entity::<deployment_holds::Entity>()?;
        if options.hold_grace <= queue.options.transaction_timeout {
            return Err(Error::Invalid);
        }
        Ok(Self {
            scheduler: Scheduler::new(queue.clone(), options.scheduling)?,
            recovery: Recovery::new(queue.clone(), options.recovery)?,
            lifecycle,
            capacity: Capacity::new(coordinator, contract, options.capacity)?,
            queue,
            options,
            cursors: Default::default(),
            next_lane: 0,
        })
    }

    /// The placement demand and capacity operations this driver runs.
    #[must_use]
    pub const fn capacity(&self) -> &Capacity {
        &self.capacity
    }

    /// Visit bounded calendar, recovery, transitional-hold, closing and
    /// placement pages independently.
    ///
    /// A lane's deadline covers its scans and candidate operations together.
    /// Failures never prevent another lane's turn. The retention lane resumes
    /// unfinished hold intents and releases holds no enabled selection or
    /// unsettled job needs once they are older than `Options::hold_grace`.
    /// Dropping this future advances past an attempted candidate without deleting
    /// its durable work. A dispatched commit may remain uncertain until replay.
    pub async fn tick(&mut self) -> TickReport {
        let mut report = TickReport::default();
        for _ in 0..self.cursors.len() {
            let lane = self.next_lane;
            self.next_lane = (lane + 1) % self.cursors.len();
            let result = self.lane(lane).await;
            match lane {
                0 => report.scheduling = result,
                1 => report.reconciliation = result,
                2 => report.collection = result,
                3 => report.retention = result,
                4 => report.closing = result,
                5 => report.placement = result,
                6 => report.unplaced = result,
                _ => report.capacity = result,
            }
        }
        report
    }

    async fn lane(&mut self, lane: usize) -> LaneReport {
        let deadline = Deadline(Instant::now() + self.options.lane_timeout);
        let page = if lane < 4 {
            self.maintenance_page(lane, deadline).await
        } else if lane == 4 {
            self.closing_candidates(deadline).await.map(whole)
        } else {
            Box::pin(self.placement_page(lane, deadline)).await
        };
        let mut report = LaneReport::default();
        let (page, fetched) = match page {
            Ok(page) => page,
            Err(error) => {
                report.scan_error = Some(error);
                report.timed_out = deadline.expired();
                return report;
            }
        };
        for candidate in &page {
            if deadline.expired() {
                report.timed_out = true;
                break;
            }
            let id = candidate.id();
            self.cursors[lane].after = Some(id.to_owned());
            report.visited += 1;
            match deadline.run(self.dispatch(candidate)).await {
                Ok(()) => report.completed += 1,
                Err(error) => report.failures.push(CandidateFailure {
                    id: id.to_owned(),
                    error,
                }),
            }
            if deadline.expired() {
                report.timed_out = true;
                break;
            }
        }
        report.unvisited = page.len() - report.visited;
        let cursor = &mut self.cursors[lane];
        if !report.timed_out
            && (fetched < self.options.page_limit as usize || cursor.after == cursor.upper)
        {
            *cursor = Cursor::default();
            report.sweep_complete = true;
        }
        report
    }

    /// Calendar, recovery and retention pages. Each page reports the rows its
    /// scan fetched; a full fetch means the sweep has more to visit.
    async fn maintenance_page(
        &mut self,
        lane: usize,
        deadline: Deadline,
    ) -> Result<(Vec<Candidate>, usize), Error> {
        let limit = self.options.page_limit;
        let cursor = &mut self.cursors[lane];
        match lane {
            0 => scan_schedules(&self.queue, cursor, deadline, limit)
                .await
                .map(|rows| whole(rows.into_iter().map(Candidate::Scheduled).collect())),
            1 | 2 => scan::<_, Recoverable>(
                &self.queue,
                cursor,
                deadline,
                limit,
                recovery_duties::app_id,
                |now| {
                    Ok(recovery_duties::kind
                        .eq(lane_kind(lane).as_str())?
                        .and(recovery_duties::next_due_at.lte(now)?))
                },
            )
            .await
            .map(|rows| {
                whole(
                    rows.into_iter()
                        .map(|row| Candidate::Recoverable(lane_kind(lane), row))
                        .collect(),
                )
            }),
            3 => scan_holds(&self.queue, cursor, deadline, limit, self.options.hold_grace)
                .await
                .map(|rows| whole(rows.into_iter().map(Candidate::Hold).collect())),
            _ => Err(Error::Invalid),
        }
    }

    /// The placement lanes' pages: claimable jobs by app, recorded demand,
    /// and capacity requests. A deduplicated page reports the rows its scan
    /// fetched, which can exceed the candidates it yields.
    async fn placement_page(
        &mut self,
        lane: usize,
        deadline: Deadline,
    ) -> Result<(Vec<Candidate>, usize), Error> {
        let declarative = matches!(self.capacity.contract(), Contract::Declarative(_));
        let limit = self.options.page_limit;
        let cursor = &mut self.cursors[lane];
        match lane {
            // Claimable jobs by app, including leases a dead worker let lapse.
            // Rows arrive in app order, so one app's jobs are adjacent.
            5 => scan::<_, Claimable>(&self.queue, cursor, deadline, limit, jobs::app_id, |now| {
                Ok(jobs::state
                    .eq("ready")?
                    .and(jobs::available_at.lte(now)?)
                    .or(jobs::state
                        .eq("leased")?
                        .and(jobs::lease_deadline.lte(Some(now))?)))
            })
            .await
            .map(|rows| {
                let fetched = rows.len();
                let mut apps: Vec<Candidate> = Vec::with_capacity(fetched);
                for row in rows {
                    if !matches!(apps.last(), Some(Candidate::Visit(last)) if *last == row.app_id)
                    {
                        apps.push(Candidate::Visit(row.app_id));
                    }
                }
                (apps, fetched)
            }),
            6 if declarative => scan::<_, Demanded>(
                &self.queue,
                cursor,
                deadline,
                limit,
                capacity_demands::id,
                |_| Ok(Filter::all()),
            )
            .await
            .map(|rows| whole(rows.into_iter().map(|row| Candidate::Visit(row.id)).collect())),
            6 => scan::<_, Intended>(
                &self.queue,
                cursor,
                deadline,
                limit,
                capacity_intents::id,
                |_| Ok(capacity_intents::state.ne(IntentState::Settled.as_str())?),
            )
            .await
            .map(|rows| whole(rows.into_iter().map(|row| Candidate::Visit(row.id)).collect())),
            _ if declarative => scan::<_, Targeted>(
                &self.queue,
                cursor,
                deadline,
                limit,
                capacity_targets::id,
                |_| Ok(Filter::all()),
            )
            .await
            .map(|rows| whole(rows.into_iter().map(|row| Candidate::Zone(row.id)).collect())),
            _ => scan::<_, Intended>(
                &self.queue,
                cursor,
                deadline,
                limit,
                capacity_intents::id,
                |_| {
                    Ok(capacity_intents::state
                        .ne(IntentState::Settled.as_str())?
                        .and(capacity_intents::state.ne(IntentState::Provisioned.as_str())?))
                },
            )
            .await
            .map(|rows| whole(rows.into_iter().map(|row| Candidate::Intent(row.id)).collect())),
        }
    }

    async fn dispatch(&self, candidate: &Candidate) -> Result<(), Error> {
        match candidate {
            Candidate::Scheduled(row) => {
                let app = AppId::parse(&row.app_id).map_err(|_| Error::Storage)?;
                let schedule = ScheduleId::parse(&row.id).map_err(|_| Error::Storage)?;
                self.scheduler.dispatch(&app, &schedule).await?;
            }
            Candidate::Recoverable(kind, row) => {
                let app = AppId::parse(&row.app_id).map_err(|_| Error::Storage)?;
                self.recovery.dispatch(&app, *kind).await?;
            }
            Candidate::Hold(row) => {
                let app = AppId::parse(&row.app_id).map_err(|_| Error::Storage)?;
                let deployment =
                    DeploymentId::parse(&row.deployment_id).map_err(|_| Error::Storage)?;
                self.queue
                    .maintain_deployment(&app, &deployment, self.options.hold_grace)
                    .await?;
            }
            Candidate::Closing { id, deleted } => {
                let app = AppId::parse(id).map_err(|_| Error::Storage)?;
                if *deleted {
                    self.recovery.abandon(&app).await?;
                } else {
                    self.recovery.closing_turn(&app).await?;
                }
            }
            Candidate::Visit(app) => {
                let app = AppId::parse(app).map_err(|_| Error::Storage)?;
                self.capacity.visit(&app).await?;
            }
            Candidate::Zone(zone) => {
                self.capacity.reconcile(&ZoneId::parse(zone)?).await?;
            }
            Candidate::Intent(app) => {
                let app = AppId::parse(app).map_err(|_| Error::Storage)?;
                self.capacity.request(&app).await?;
            }
        }
        Ok(())
    }

    /// A page of closing candidates with Control's deletion state, read once
    /// for the page under the lane deadline.
    async fn closing_candidates(&mut self, deadline: Deadline) -> Result<Vec<Candidate>, Error> {
        let ids = scan_closing(
            &self.recovery,
            &mut self.cursors[4],
            deadline,
            self.options.page_limit,
        )
        .await?;
        let apps: Vec<AppId> = ids.iter().filter_map(|id| AppId::parse(id).ok()).collect();
        let deleted: BTreeSet<AppId> = deadline.run(self.lifecycle.deleted(&apps)).await?;
        Ok(ids
            .into_iter()
            .map(|id| Candidate::Closing {
                deleted: AppId::parse(&id).is_ok_and(|app| deleted.contains(&app)),
                id,
            })
            .collect())
    }
}

#[derive(Clone, Copy)]
struct Deadline(Instant);

impl Deadline {
    fn expired(self) -> bool {
        Instant::now() >= self.0
    }

    async fn run<T>(self, future: impl Future<Output = Result<T, Error>>) -> Result<T, Error> {
        if self.expired() {
            return Err(Error::Timeout);
        }
        let result =
            compio::time::timeout(self.0.saturating_duration_since(Instant::now()), future)
                .await
                .unwrap_or(Err(Error::Timeout));
        if self.expired() {
            Err(Error::Timeout)
        } else {
            result
        }
    }
}

trait ScanRow {
    fn id(&self) -> &str;
}

impl ScanRow for Scheduled {
    fn id(&self) -> &str {
        &self.id
    }
}

#[derive(FromRow)]
#[orm(entity = recovery_duties)]
struct Recoverable {
    app_id: String,
}
impl ScanRow for Recoverable {
    fn id(&self) -> &str {
        &self.app_id
    }
}

#[derive(FromRow)]
#[orm(entity = deployment_holds)]
struct Hold {
    id: String,
    app_id: String,
    deployment_id: String,
}
impl ScanRow for Hold {
    fn id(&self) -> &str {
        &self.id
    }
}

#[derive(FromRow)]
#[orm(entity = jobs)]
struct Claimable {
    app_id: String,
}
impl ScanRow for Claimable {
    fn id(&self) -> &str {
        &self.app_id
    }
}

#[derive(FromRow)]
#[orm(entity = capacity_demands)]
struct Demanded {
    id: String,
}
impl ScanRow for Demanded {
    fn id(&self) -> &str {
        &self.id
    }
}

#[derive(FromRow)]
#[orm(entity = capacity_intents)]
struct Intended {
    id: String,
}
impl ScanRow for Intended {
    fn id(&self) -> &str {
        &self.id
    }
}

#[derive(FromRow)]
#[orm(entity = capacity_targets)]
struct Targeted {
    id: String,
}
impl ScanRow for Targeted {
    fn id(&self) -> &str {
        &self.id
    }
}

enum Candidate {
    Scheduled(Scheduled),
    Recoverable(DutyKind, Recoverable),
    Hold(Hold),
    Closing { id: String, deleted: bool },
    /// An app to give an owner or to clear from demand.
    Visit(String),
    /// An execution zone whose capacity target to reconcile.
    Zone(String),
    /// An app whose comparison-contract intent to request.
    Intent(String),
}
impl Candidate {
    fn id(&self) -> &str {
        match self {
            Self::Scheduled(row) => row.id(),
            Self::Recoverable(_, row) => row.id(),
            Self::Hold(row) => row.id(),
            Self::Closing { id, .. } => id,
            Self::Visit(id) | Self::Zone(id) | Self::Intent(id) => id,
        }
    }
}

/// A page visited exactly as fetched.
const fn whole(candidates: Vec<Candidate>) -> (Vec<Candidate>, usize) {
    let fetched = candidates.len();
    (candidates, fetched)
}

const fn lane_kind(lane: usize) -> DutyKind {
    if lane == 1 {
        DutyKind::Reconcile
    } else {
        DutyKind::Collect
    }
}

async fn scan_schedules(
    queue: &Queue,
    cursor: &mut Cursor,
    deadline: Deadline,
    limit: u32,
) -> Result<Vec<Scheduled>, Error> {
    if cursor.upper.is_none() {
        let upper = deadline
            .run(queue.transact(|tx| async move {
                scheduling::due_in(&tx, queue.clock.now().await?, None, None, true, 1).await
            }))
            .await?;
        cursor.upper = upper.into_iter().next().map(|row| row.id);
    }
    let Some(upper) = cursor.upper.as_deref() else {
        return Ok(Vec::new());
    };
    let after = cursor.after.as_deref();
    deadline
        .run(queue.transact(|tx| async move {
            scheduling::due_in(
                &tx,
                queue.clock.now().await?,
                after,
                Some(upper),
                false,
                limit,
            )
            .await
        }))
        .await
}

async fn scan_closing(
    recovery: &Recovery,
    cursor: &mut Cursor,
    deadline: Deadline,
    limit: u32,
) -> Result<Vec<String>, Error> {
    if cursor.upper.is_none() {
        let upper = deadline
            .run(recovery.closing_page(None, None, true, 1))
            .await?;
        cursor.upper = upper.into_iter().next();
    }
    let Some(upper) = cursor.upper.as_deref() else {
        return Ok(Vec::new());
    };
    deadline
        .run(recovery.closing_page(cursor.after.as_deref(), Some(upper), false, limit))
        .await
}

async fn scan<C, R>(
    queue: &Queue,
    cursor: &mut Cursor,
    deadline: Deadline,
    limit: u32,
    key: Field<C>,
    filter: impl Fn(i64) -> Result<Filter<C::Entity>, Error>,
) -> Result<Vec<R>, Error>
where
    C: FilterableColumn<SqlType = Text>,
    R: FromRow<C::Entity> + ScanRow,
{
    if cursor.upper.is_none() {
        let filter = &filter;
        let upper = deadline
            .run(queue.transact(|tx| async move {
                Ok(tx
                    .entity::<C::Entity>()?
                    .query()
                    .filter(filter(queue.clock.now().await?)?)
                    .order_by(key.desc())
                    .first::<R>()
                    .await?)
            }))
            .await?;
        cursor.upper = upper.map(|row| row.id().to_owned());
    }
    let Some(upper) = cursor.upper.as_deref() else {
        return Ok(Vec::new());
    };
    let after = cursor.after.as_deref();
    deadline
        .run(queue.transact(|tx| async move {
            let mut filter = filter(queue.clock.now().await?)?.and(key.lte(upper)?);
            if let Some(after) = after {
                filter = filter.and(key.gt(after)?);
            }
            Ok(tx
                .entity::<C::Entity>()?
                .query()
                .filter(filter)
                .order_by(key.asc())
                .limit(i64::from(limit))?
                .all::<R>()
                .await?)
        }))
        .await
}

async fn scan_holds(
    queue: &Queue,
    cursor: &mut Cursor,
    deadline: Deadline,
    limit: u32,
    grace: Duration,
) -> Result<Vec<Hold>, Error> {
    let grace = i64::try_from(grace.as_millis()).map_err(|_| Error::Invalid)?;
    if cursor.upper.is_none() {
        let upper = deadline
            .run(queue.transact(|tx| async move {
                let cutoff = queue.clock.now().await?.saturating_sub(grace);
                retention::maintenance_in(&tx, cutoff, None, None, true, 1).await
            }))
            .await?;
        cursor.upper = upper.into_iter().next().map(|row: Hold| row.id);
    }
    let Some(upper) = cursor.upper.as_deref() else {
        return Ok(Vec::new());
    };
    let after = cursor.after.as_deref();
    deadline
        .run(queue.transact(|tx| async move {
            let cutoff = queue.clock.now().await?.saturating_sub(grace);
            retention::maintenance_in(&tx, cutoff, after, Some(upper), false, limit).await
        }))
        .await
}
