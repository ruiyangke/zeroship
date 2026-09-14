//! Bounded manager maintenance, independent of workers and creator storage.
#![expect(
    clippy::future_not_send,
    reason = "driver operations share their queue's owning compio runtime"
)]

use crate::{
    models::{
        recovery_duties,
        schema::{deployment_holds, schedules},
    },
    recovery::{self, DutyKind, Recovery},
    scheduling::{self, Due as Scheduled, Scheduler},
    Error, Queue,
};
use std::{
    future::Future,
    time::{Duration, Instant},
};
use zeroship_core::{app_id::AppId, workflow_jobs::DeploymentId, workflow_schedules::ScheduleId};
use zeroship_data_orm::orm::{sql_types::Text, Field, Filter, FilterableColumn, FromRow};

#[derive(Clone, Copy, Debug)]
pub struct Options {
    pub scheduling: scheduling::Options,
    pub recovery: recovery::Options,
    /// Maximum candidate records visited in each lane's turn.
    pub page_limit: u32,
    /// Shared deadline for a lane's scans and entire candidate page.
    pub lane_timeout: Duration,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            scheduling: scheduling::Options::default(),
            recovery: recovery::Options::default(),
            page_limit: 128,
            lane_timeout: Duration::from_secs(10),
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
            || self.scheduling.max_schedules == 0
            || self.scheduling.max_backfill == 0
            || i64::try_from(self.scheduling.max_backfill).is_err()
            || self.scheduling.min_interval_ms <= 0
            || self.scheduling.page_size == 0
            || i64::from(self.scheduling.page_size) > zeroship_data_orm::sql::MAX_ROW_LIMIT
            || self.recovery.interval.as_millis() == 0
            || i64::try_from(self.recovery.interval.as_millis()).is_err()
            || self.recovery.page_size == 0
            || i64::from(self.recovery.page_size) > zeroship_data_orm::sql::MAX_ROW_LIMIT
        {
            return Err(Error::Invalid);
        }
        Ok(())
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
}

/// A host calls `tick` and owns cadence, cancellation and shutdown.
///
/// Cursors are disposable scan positions, not authority or durable work. Failed
/// candidates advance within a captured identity range and retry on another sweep.
/// Existing operation transactions own publication, retention and commit fences.
/// No placement or worker is required to generate due jobs.
#[derive(Debug)]
pub struct Driver {
    queue: Queue,
    scheduler: Scheduler,
    recovery: Recovery,
    options: Options,
    cursors: [Cursor; 4],
    next_lane: usize,
}

#[derive(Debug, Default)]
struct Cursor {
    after: Option<String>,
    upper: Option<String>,
}

impl Driver {
    /// Construct every maintenance operation over the same platform queue.
    ///
    /// # Errors
    /// Rejects invalid maintenance options or incompatible native metadata.
    pub fn new(queue: Queue, options: Options) -> Result<Self, Error> {
        options.validate()?;
        queue.database.entity::<schedules::Entity>()?;
        queue.database.entity::<recovery_duties::Entity>()?;
        queue.database.entity::<deployment_holds::Entity>()?;
        Ok(Self {
            scheduler: Scheduler::new(queue.clone(), options.scheduling)?,
            recovery: Recovery::new(queue.clone(), options.recovery)?,
            queue,
            options,
            cursors: Default::default(),
            next_lane: 0,
        })
    }

    /// Visit bounded calendar, recovery and transitional-hold pages independently.
    ///
    /// A lane's deadline covers its scans and candidate operations together.
    /// Failures never prevent another lane's turn. Held deployments require the
    /// host's explicit release policy; this driver only resumes acquiring/releasing.
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
                _ => report.retention = result,
            }
        }
        report
    }

    async fn lane(&mut self, lane: usize) -> LaneReport {
        let deadline = Deadline(Instant::now() + self.options.lane_timeout);
        let page = match lane {
            0 => scan_schedules(
                &self.queue,
                &mut self.cursors[lane],
                deadline,
                self.options.page_limit,
            )
            .await
            .map(|rows| rows.into_iter().map(Candidate::Scheduled).collect()),
            1 | 2 => scan::<_, Recoverable>(
                &self.queue,
                &mut self.cursors[lane],
                deadline,
                self.options.page_limit,
                recovery_duties::app_id,
                |now| {
                    Ok(recovery_duties::kind
                        .eq(lane_kind(lane).as_str())?
                        .and(recovery_duties::next_due_at.lte(now)?))
                },
            )
            .await
            .map(|rows| {
                rows.into_iter()
                    .map(|row| Candidate::Recoverable(lane_kind(lane), row))
                    .collect()
            }),
            _ => scan::<_, Hold>(
                &self.queue,
                &mut self.cursors[lane],
                deadline,
                self.options.page_limit,
                deployment_holds::id,
                |_| {
                    Ok(deployment_holds::state
                        .eq("acquiring")?
                        .or(deployment_holds::state.eq("releasing")?))
                },
            )
            .await
            .map(|rows| rows.into_iter().map(Candidate::Hold).collect()),
        };
        let mut report = LaneReport::default();
        let page: Vec<Candidate> = match page {
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
            && (page.len() < self.options.page_limit as usize || cursor.after == cursor.upper)
        {
            *cursor = Cursor::default();
            report.sweep_complete = true;
        }
        report
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
                self.queue.reconcile_deployment(&app, &deployment).await?;
            }
        }
        Ok(())
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

enum Candidate {
    Scheduled(Scheduled),
    Recoverable(DutyKind, Recoverable),
    Hold(Hold),
}
impl Candidate {
    fn id(&self) -> &str {
        match self {
            Self::Scheduled(row) => row.id(),
            Self::Recoverable(_, row) => row.id(),
            Self::Hold(row) => row.id(),
        }
    }
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
