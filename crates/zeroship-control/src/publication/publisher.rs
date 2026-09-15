//! Deliver pending lifecycle intents to the workflow manager.
//!
//! Each app's intents are published strictly in revision order: the manager
//! refuses an unknown revision below one it has accepted, so sending a later
//! intent first would strand the earlier one. Manager calls run outside any
//! database transaction; a fresh transaction then records only the receipt
//! the manager returned for that exact request. A lost reply, a timeout, a
//! refusal or a failed confirmation leaves the intent pending, and the next
//! attempt resends the same revision, which the manager replays exactly.
//!
//! An app whose attempt fails is not attempted again until its retry delay
//! has passed. The delay doubles with each consecutive failure up to a cap and
//! resets after a success, while every other app keeps publishing. A failure
//! is therefore logged once per attempt, not once per pass.

use super::catalog::{self, CatalogError, ACKNOWLEDGED, ACTIVATE, DISABLE, PENDING};
use super::models::catalog::app_lifecycle_intents as intents;
use super::shared::{Catalog, Closing};
use futures::future::{self, Either};
use std::{
    collections::{HashMap, HashSet},
    future::Future,
    pin::{pin, Pin},
    sync::Arc,
    time::{Duration, Instant},
};
use zeroship_core::{
    app_id::AppId,
    service_peers::ServiceAuth,
    workflow_coordination::Revision,
    workflow_jobs::{DeploymentId, JobOperation, JobSpec},
    workflow_schedules::{ActivateSchedules, DisableSchedules, RegisterSchedules},
};
use zeroship_data_orm::orm::{Database, FromRow};
use zeroship_workflow_client::{ControlCoordinator, Error as ManagerError, Options, Transport};

/// A boxed manager exchange, local to the publisher's compio thread.
pub type Exchange<'a, T> = Pin<Box<dyn Future<Output = Result<T, ManagerError>> + 'a>>;

/// The manager's exact-Control schedule publication routes. The production
/// implementation is [`ControlCoordinator`], whose calls validate each reply
/// against its request before returning it.
pub trait ScheduleManager {
    fn register<'a>(&'a self, request: &'a RegisterSchedules) -> Exchange<'a, RegisterSchedules>;
    fn activate<'a>(&'a self, request: &'a ActivateSchedules) -> Exchange<'a, JobSpec>;
    fn disable<'a>(&'a self, request: &'a DisableSchedules) -> Exchange<'a, DisableSchedules>;
}

impl ScheduleManager for ControlCoordinator {
    fn register<'a>(&'a self, request: &'a RegisterSchedules) -> Exchange<'a, RegisterSchedules> {
        Box::pin(self.register_schedules(request))
    }
    fn activate<'a>(&'a self, request: &'a ActivateSchedules) -> Exchange<'a, JobSpec> {
        Box::pin(self.activate_schedules(request))
    }
    fn disable<'a>(&'a self, request: &'a DisableSchedules) -> Exchange<'a, DisableSchedules> {
        Box::pin(self.disable_schedules(request))
    }
}

/// Page, attempt and pacing bounds of the publisher.
#[derive(Debug, Clone, Copy)]
pub struct PublisherConfig {
    /// Pending intents read per pass.
    pub batch_size: i64,
    /// Bound on one intent's manager exchange and its confirmation.
    pub attempt_timeout: Duration,
    /// Pause between passes that read their page.
    pub interval: Duration,
    /// Delay before an app whose attempt failed is attempted again, and the
    /// pause after a pass that could not read its page.
    pub retry_initial: Duration,
    /// Cap of a retry delay, which doubles with each consecutive failure.
    pub retry_max: Duration,
}

impl Default for PublisherConfig {
    fn default() -> Self {
        Self {
            batch_size: 128,
            attempt_timeout: Duration::from_secs(30),
            interval: Duration::from_secs(1),
            retry_initial: Duration::from_secs(1),
            retry_max: Duration::from_secs(60),
        }
    }
}

impl PublisherConfig {
    /// Check that every bound can be honoured.
    ///
    /// # Errors
    /// Names the first bound that cannot.
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.batch_size <= 0 || self.batch_size > zeroship_data_orm::sql::MAX_ROW_LIMIT {
            return Err("the publication batch size must be positive and within the row limit");
        }
        if self.attempt_timeout.is_zero() {
            return Err("the publication attempt timeout must be positive");
        }
        if self.interval.is_zero() {
            return Err("the publication interval must be positive");
        }
        if self.retry_initial.is_zero() {
            return Err("the first publication retry delay must be positive");
        }
        if self.retry_max < self.retry_initial {
            return Err("the publication retry cap must not be below the first retry delay");
        }
        if Instant::now().checked_add(self.retry_max).is_none() {
            return Err("the publication retry cap is out of range");
        }
        Ok(())
    }

    /// The delay after a failure that followed a delay of `previous`, if any.
    fn next_delay(&self, previous: Option<Duration>) -> Duration {
        previous
            .map_or(self.retry_initial, |delay| delay.saturating_mul(2))
            .min(self.retry_max)
    }
}

/// What one pass did. `deferred` counts intents left behind an earlier
/// failure of the same app in this pass; `waiting` counts intents of apps
/// whose retry delay has not passed.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct PublisherStats {
    pub attempted: usize,
    pub acknowledged: usize,
    pub failed: usize,
    pub deferred: usize,
    pub waiting: usize,
}

/// The retry an app is waiting for after a failed attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Retry {
    /// The delay since the failed attempt.
    pub delay: Duration,
    /// The earliest pass that attempts the app again.
    pub at: Instant,
}

/// Why an intent stayed pending.
#[derive(Debug, thiserror::Error)]
pub enum PublishError {
    #[error("workflow manager exchange failed: {0}")]
    Manager(ManagerError),
    #[error("lifecycle publication attempt timed out")]
    Timeout,
    #[error("stored lifecycle intent is invalid: {0}")]
    InvalidIntent(&'static str),
    #[error("lifecycle confirmation failed: {0}")]
    Catalog(CatalogError),
}

#[derive(Debug, Clone, FromRow)]
#[orm(entity = intents)]
struct PendingIntent {
    id: String,
    app_id: String,
    revision: i64,
    action: String,
    deploy_id: Option<String>,
    registration: Option<String>,
}

#[derive(FromRow)]
#[orm(entity = intents)]
struct StoredIntent {
    app_id: String,
    revision: i64,
    action: String,
    deploy_id: Option<String>,
    registration: Option<String>,
    state: String,
    receipt: Option<String>,
}

/// A rotating publication driver over Control's pending intents.
pub struct Publisher<M> {
    database: Database,
    manager: M,
    config: PublisherConfig,
    after: Option<String>,
    retries: HashMap<String, Retry>,
}

impl<M> std::fmt::Debug for Publisher<M> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Publisher")
            .field("config", &self.config)
            .field("after", &self.after)
            .field("retries", &self.retries)
            .finish_non_exhaustive()
    }
}

impl<M> Publisher<M> {
    /// The retry `app` is waiting for, if its last attempt failed.
    #[must_use]
    pub fn retry(&self, app: &AppId) -> Option<Retry> {
        self.retries.get(app.as_str()).copied()
    }
}

impl<M: ScheduleManager> Publisher<M> {
    /// Bind a Control catalog database opened by [`catalog::connect`].
    ///
    /// # Errors
    /// Refuses invalid bounds and missing catalog models.
    pub fn new(
        database: Database,
        manager: M,
        config: PublisherConfig,
    ) -> Result<Self, CatalogError> {
        config.validate().map_err(CatalogError::Storage)?;
        database.entity::<intents::Entity>()?;
        Ok(Self {
            database,
            manager,
            config,
            after: None,
            retries: HashMap::new(),
        })
    }

    /// Publish one bounded page now. See [`Self::tick_at`].
    ///
    /// # Errors
    /// Reports a failed page read.
    pub async fn tick(&mut self) -> Result<PublisherStats, CatalogError> {
        self.tick_at(Instant::now()).await
    }

    /// Publish one bounded page as of `now`. Pending intents are read in
    /// `(app, revision)` order. An app waiting out a retry delay is skipped;
    /// an app's failure defers its later intents and schedules its retry; and
    /// the cursor then moves to the next app so a blocked app cannot starve
    /// the others.
    ///
    /// # Errors
    /// Reports a failed page read. Individual failures leave their intents
    /// pending and are counted in the returned statistics.
    pub async fn tick_at(&mut self, now: Instant) -> Result<PublisherStats, CatalogError> {
        let from_start = self.after.is_none();
        let mut filter = intents::state.eq(PENDING)?;
        if let Some(after) = &self.after {
            filter = filter.and(intents::app_id.gt(after.as_str())?);
        }
        let page = compio::time::timeout(
            self.config.attempt_timeout,
            self.database
                .entity::<intents::Entity>()?
                .query()
                .filter(filter)
                .order_by(intents::app_id.asc())
                .order_by(intents::revision.asc())
                .limit(self.config.batch_size)?
                .all::<PendingIntent>(),
        )
        .await
        .map_err(|_| CatalogError::Storage("pending intent page timed out"))??;
        let mut stats = PublisherStats::default();
        let mut blocked: Option<&str> = None;
        for intent in &page {
            let app = intent.app_id.as_str();
            if blocked == Some(app) {
                stats.deferred += 1;
                continue;
            }
            if self.retries.get(app).is_some_and(|retry| now < retry.at) {
                stats.waiting += 1;
                continue;
            }
            stats.attempted += 1;
            let outcome = compio::time::timeout(self.config.attempt_timeout, self.publish(intent))
                .await
                .unwrap_or(Err(PublishError::Timeout));
            match outcome {
                Ok(()) => {
                    stats.acknowledged += 1;
                    self.retries.remove(app);
                }
                Err(error) => {
                    stats.failed += 1;
                    blocked = Some(app);
                    let delay = self
                        .config
                        .next_delay(self.retries.get(app).map(|retry| retry.delay));
                    self.retries.insert(
                        intent.app_id.clone(),
                        Retry {
                            delay,
                            at: now + delay,
                        },
                    );
                    tracing::warn!(app_id = %intent.app_id, revision = intent.revision,
                        action = %intent.action, %error, retry_in = ?delay,
                        "lifecycle intent remains pending");
                }
            }
        }
        let full = usize::try_from(self.config.batch_size)
            .map_err(|_| CatalogError::Storage("invalid publication bounds"))?;
        if page.len() < full {
            if from_start {
                // This page held every pending intent, so an app absent from
                // it has nothing left to retry.
                let pending: HashSet<&str> =
                    page.iter().map(|intent| intent.app_id.as_str()).collect();
                self.retries.retain(|app, _| pending.contains(app.as_str()));
            }
            self.after = None;
        } else {
            self.after = page.last().map(|intent| intent.app_id.clone());
        }
        Ok(stats)
    }

    async fn publish(&self, intent: &PendingIntent) -> Result<(), PublishError> {
        let app = AppId::parse(&intent.app_id).map_err(|_| PublishError::InvalidIntent("app"))?;
        let revision = Revision::try_from(intent.revision)
            .map_err(|_| PublishError::InvalidIntent("revision"))?;
        let receipt = match (
            intent.action.as_str(),
            intent.deploy_id.as_deref(),
            intent.registration.as_deref(),
        ) {
            (ACTIVATE, Some(deployment), Some(registration)) => {
                let deployment = DeploymentId::parse(deployment)
                    .map_err(|_| PublishError::InvalidIntent("deployment"))?;
                let registration: RegisterSchedules = serde_json::from_str(registration)
                    .map_err(|_| PublishError::InvalidIntent("registration"))?;
                if registration.app_id != app || registration.deployment_id != deployment {
                    return Err(PublishError::InvalidIntent("registration scope"));
                }
                self.manager
                    .register(&registration)
                    .await
                    .map_err(PublishError::Manager)?;
                let request = ActivateSchedules {
                    app_id: app,
                    deployment_id: deployment,
                    revision,
                };
                let job = self
                    .manager
                    .activate(&request)
                    .await
                    .map_err(PublishError::Manager)?;
                if !activates(&job, &request) {
                    return Err(PublishError::Manager(ManagerError::InvalidResponse));
                }
                serde_json::to_string(&job).map_err(|_| PublishError::InvalidIntent("receipt"))?
            }
            (DISABLE, None, None) => {
                let request = DisableSchedules {
                    app_id: app,
                    revision,
                };
                let receipt = self
                    .manager
                    .disable(&request)
                    .await
                    .map_err(PublishError::Manager)?;
                if receipt != request {
                    return Err(PublishError::Manager(ManagerError::InvalidResponse));
                }
                serde_json::to_string(&receipt)
                    .map_err(|_| PublishError::InvalidIntent("receipt"))?
            }
            _ => return Err(PublishError::InvalidIntent("action")),
        };
        let now = super::now_millis();
        catalog::transact(&self.database, |tx| async move {
            confirm(&tx, intent, &receipt, now).await
        })
        .await
        .map_err(PublishError::Catalog)
    }
}

/// The receipt the manager returns for an activation names that exact app,
/// deployment and revision.
fn activates(job: &JobSpec, request: &ActivateSchedules) -> bool {
    job.app_id == request.app_id
        && job.operation
            == (JobOperation::Activate {
                deployment_id: request.deployment_id.clone(),
                revision: request.revision,
            })
}

/// Record the manager's receipt for the pending intent it answers. A replica
/// that already confirmed the same receipt makes this a no-op; a different
/// receipt for an acknowledged intent is a storage failure, never overwritten.
async fn confirm(
    tx: &Database,
    intent: &PendingIntent,
    receipt: &str,
    now: i64,
) -> Result<(), CatalogError> {
    let entity = tx.entity::<intents::Entity>()?;
    let updated = entity
        .update_many(
            intents::id
                .eq(intent.id.as_str())?
                .and(intents::state.eq(PENDING)?),
            intents::state
                .set(ACKNOWLEDGED)?
                .and(intents::receipt.set(Some(receipt))?)?
                .and(intents::acknowledged_at.set(Some(now))?)?,
        )
        .await?;
    let stored = entity
        .query()
        .filter(intents::id.eq(intent.id.as_str())?)
        .first::<StoredIntent>()
        .await?
        .ok_or(CatalogError::Storage("confirmed intent vanished"))?;
    if stored.app_id != intent.app_id
        || stored.revision != intent.revision
        || stored.action != intent.action
        || stored.deploy_id != intent.deploy_id
        || stored.registration != intent.registration
    {
        return Err(CatalogError::Storage("confirmed intent changed"));
    }
    let recorded = stored
        .receipt
        .as_deref()
        .ok_or(CatalogError::Storage("acknowledged intent has no receipt"))?;
    if updated > 1
        || stored.state != ACKNOWLEDGED
        || !same_receipt(&intent.action, recorded, receipt)
    {
        return Err(CatalogError::Storage(
            "a different manager receipt is recorded for this intent",
        ));
    }
    Ok(())
}

fn same_receipt(action: &str, recorded: &str, received: &str) -> bool {
    match action {
        ACTIVATE => matches!(
            (serde_json::from_str::<JobSpec>(recorded), serde_json::from_str::<JobSpec>(received)),
            (Ok(recorded), Ok(received)) if recorded == received
        ),
        DISABLE => matches!(
            (
                serde_json::from_str::<DisableSchedules>(recorded),
                serde_json::from_str::<DisableSchedules>(received),
            ),
            (Ok(recorded), Ok(received)) if recorded == received
        ),
        _ => false,
    }
}

/// Why Control cannot publish lifecycle intents. Each is a refusal to start:
/// a Control that accepted deploys it could never publish would leave them
/// pending forever.
#[derive(Debug, thiserror::Error)]
pub enum StartError {
    #[error(
        "Control has no Control service signer, and the workflow manager refuses unsigned \
         schedule publication; configure control.service_key_file and control.service_peers_file"
    )]
    Unsigned,
    #[error(
        "control.workflow_coordinator_url is not a usable workflow coordinator origin ({0}); \
         use an https origin without path, query or credentials, or http on a loopback address"
    )]
    Coordinator(ManagerError),
    #[error("invalid lifecycle publisher bounds: {0}")]
    Config(&'static str),
    #[error("the control catalog could not start the lifecycle publisher: {0}")]
    Catalog(CatalogError),
}

/// Check the coordinator origin publication would use, without keys, sockets
/// or a client, so configuration checks refuse it before anything starts.
///
/// # Errors
/// Refuses an origin the manager client would refuse.
pub fn validate_coordinator(url: &str) -> Result<(), StartError> {
    Transport::validate_config(url, Options::default()).map_err(StartError::Coordinator)
}

/// Start delivering lifecycle intents on one of the shared catalog's threads,
/// with that thread's database, until the catalog closes.
///
/// # Errors
/// Refuses invalid bounds, a coordinator origin the manager client refuses,
/// and a Control without a Control service signer, before any intent is read.
pub async fn start(
    catalog: &Catalog,
    service_auth: Arc<ServiceAuth>,
    coordinator_url: &str,
    config: PublisherConfig,
) -> Result<(), StartError> {
    config.validate().map_err(StartError::Config)?;
    validate_coordinator(coordinator_url)?;
    // Built here to refuse, and again on the catalog thread to use: the HTTP
    // client's pooled streams belong to the thread that opens them.
    manager(coordinator_url, service_auth.clone())?;
    let url = coordinator_url.to_owned();
    catalog
        .spawn(move |database, closing| {
            let manager = manager(&url, service_auth)
                .map_err(|_| CatalogError::Storage("the workflow manager client was refused"))?;
            let publisher = Publisher::new(database, manager, config)?;
            Ok(Box::pin(serve(publisher, closing)))
        })
        .await
        .map_err(StartError::Catalog)
}

/// The manager client publication would use. Building one is the signer
/// check: the client refuses a missing signer and a signer that is not
/// Control's, which are the two ways the schedule routes are unreachable.
fn manager(url: &str, auth: Arc<ServiceAuth>) -> Result<ControlCoordinator, StartError> {
    ControlCoordinator::new(url, auth, Options::default()).map_err(|error| match error {
        ManagerError::Unauthenticated => StartError::Unsigned,
        other => StartError::Coordinator(other),
    })
}

/// Publish a pass every interval until the catalog closes. A pass that cannot
/// read its page is logged once and followed by a pause that doubles up to
/// the retry cap, so an unreachable catalog is not logged on every interval.
async fn serve<M: ScheduleManager>(mut publisher: Publisher<M>, closing: Closing) {
    let mut paused: Option<Duration> = None;
    loop {
        let pass = {
            let pass = pin!(publisher.tick());
            match future::select(pass, pin!(closing.clone().wait())).await {
                Either::Left((pass, _)) => pass,
                Either::Right(_) => return,
            }
        };
        let config = publisher.config;
        let pause = match pass {
            Ok(visited) => {
                paused = None;
                if visited.attempted != 0 {
                    tracing::info!(?visited, "lifecycle publisher visited pending intents");
                }
                config.interval
            }
            Err(error) => {
                let delay = config.next_delay(paused);
                paused = Some(delay);
                tracing::error!(%error, retry_in = ?delay, "lifecycle publication pass failed");
                delay.max(config.interval)
            }
        };
        let sleep = pin!(compio::time::sleep(pause));
        if let Either::Right(_) = future::select(sleep, pin!(closing.clone().wait())).await {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zeroship_core::{
        service_assertion::{ServiceSigningKey, ServiceTrustBundle, TransportAssertionVerifier},
        service_peers::{service_issuer, ServiceKeyring, CONTROL_SERVICE_NAME, WORKER_SERVICE_NAME},
    };

    fn signer(service: &str) -> Arc<ServiceAuth> {
        Arc::new(ServiceAuth::new(
            ServiceKeyring::from_parts(
                service_issuer(service).unwrap(),
                ServiceSigningKey::generate(),
                ServiceTrustBundle::new(),
            )
            .unwrap(),
            Arc::new(TransportAssertionVerifier::new(ServiceTrustBundle::new())),
        ))
    }

    #[test]
    fn configuration_refuses_each_bound_it_cannot_honour() {
        let usable = PublisherConfig::default();
        assert_eq!(usable.validate(), Ok(()));
        let refused = [
            PublisherConfig {
                batch_size: 0,
                ..usable
            },
            PublisherConfig {
                batch_size: zeroship_data_orm::sql::MAX_ROW_LIMIT + 1,
                ..usable
            },
            PublisherConfig {
                attempt_timeout: Duration::ZERO,
                ..usable
            },
            PublisherConfig {
                interval: Duration::ZERO,
                ..usable
            },
            PublisherConfig {
                retry_initial: Duration::ZERO,
                ..usable
            },
            PublisherConfig {
                retry_max: usable
                    .retry_initial
                    .saturating_sub(Duration::from_millis(1)),
                ..usable
            },
            PublisherConfig {
                retry_max: Duration::MAX,
                ..usable
            },
        ];
        for config in refused {
            assert!(config.validate().is_err(), "{config:?}");
        }
    }

    #[test]
    fn retry_delay_doubles_from_the_first_delay_to_the_cap() {
        let config = PublisherConfig {
            retry_initial: Duration::from_millis(250),
            retry_max: Duration::from_secs(1),
            ..PublisherConfig::default()
        };
        let mut delays = Vec::new();
        let mut previous = None;
        for _ in 0..5 {
            let delay = config.next_delay(previous);
            delays.push(delay.as_millis());
            previous = Some(delay);
        }
        assert_eq!(delays, [250, 500, 1000, 1000, 1000]);
        // Doubling a delay near the representable range stays at the cap.
        let wide = PublisherConfig {
            retry_max: Duration::MAX,
            ..config
        };
        assert_eq!(wide.next_delay(Some(Duration::MAX)), Duration::MAX);
    }

    #[test]
    fn startup_refuses_an_unsigned_control_and_an_unusable_coordinator() {
        const ORIGIN: &str = "http://127.0.0.1:9093";
        assert!(validate_coordinator(ORIGIN).is_ok());
        assert!(manager(ORIGIN, signer(CONTROL_SERVICE_NAME)).is_ok());

        assert!(matches!(
            manager(ORIGIN, Arc::new(ServiceAuth::unconfigured())),
            Err(StartError::Unsigned)
        ));
        // The schedule routes accept only Control's own signer.
        assert!(matches!(
            manager(ORIGIN, signer(WORKER_SERVICE_NAME)),
            Err(StartError::Unsigned)
        ));
        for origin in [
            "",
            "not a url",
            "ftp://127.0.0.1:9093",
            "http://coordinator.internal:9093",
            "https://coordinator.internal/manager",
            "https://user:secret@coordinator.internal",
        ] {
            assert!(
                matches!(
                    validate_coordinator(origin),
                    Err(StartError::Coordinator(ManagerError::InvalidConfig))
                ),
                "{origin}"
            );
            assert!(
                matches!(
                    manager(origin, signer(CONTROL_SERVICE_NAME)),
                    Err(StartError::Coordinator(ManagerError::InvalidConfig))
                ),
                "{origin}"
            );
        }
    }
}
