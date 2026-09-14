//! Deliver pending lifecycle intents to the workflow manager.
//!
//! Each app's intents are published strictly in revision order: the manager
//! refuses an unknown revision below one it has accepted, so sending a later
//! intent first would strand the earlier one. Manager calls run outside any
//! database transaction; a fresh transaction then records only the receipt
//! the manager returned for that exact request. A lost reply, a timeout, a
//! refusal or a failed confirmation leaves the intent pending, and the next
//! attempt resends the same revision, which the manager replays exactly.

use super::catalog::{self, CatalogError, ACKNOWLEDGED, ACTIVATE, DISABLE, PENDING};
use super::models::catalog::app_lifecycle_intents as intents;
use crate::AppState;
use std::{future::Future, pin::Pin, sync::Arc, time::Duration};
use zeroship_core::{
    app_id::AppId,
    workflow_coordination::Revision,
    workflow_jobs::{DeploymentId, JobOperation, JobSpec},
    workflow_schedules::{ActivateSchedules, DisableSchedules, RegisterSchedules},
};
use zeroship_data_orm::orm::{Database, FromRow};
use zeroship_workflow_client::{ControlCoordinator, Error as ManagerError, Options};

/// Delay between publication passes.
pub const DEFAULT_TICK: Duration = Duration::from_secs(1);

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

/// Page and attempt bounds for one publication pass.
#[derive(Debug, Clone, Copy)]
pub struct PublisherConfig {
    /// Pending intents read per pass.
    pub batch_size: i64,
    /// Bound on one intent's manager exchange and its confirmation.
    pub attempt_timeout: Duration,
}

impl Default for PublisherConfig {
    fn default() -> Self {
        Self {
            batch_size: 128,
            attempt_timeout: Duration::from_secs(30),
        }
    }
}

/// What one pass did. `deferred` counts intents left behind an earlier
/// failure of the same app.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct PublisherStats {
    pub attempted: usize,
    pub acknowledged: usize,
    pub failed: usize,
    pub deferred: usize,
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
}

impl<M> std::fmt::Debug for Publisher<M> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Publisher")
            .field("config", &self.config)
            .field("after", &self.after)
            .finish_non_exhaustive()
    }
}

impl<M: ScheduleManager> Publisher<M> {
    /// Bind a Control catalog database opened by [`catalog::connect`].
    ///
    /// # Errors
    /// Refuses invalid bounds and missing catalog models.
    pub fn new(database: Database, manager: M, config: PublisherConfig) -> Result<Self, CatalogError> {
        if config.batch_size <= 0
            || config.batch_size > zeroship_data_orm::sql::MAX_ROW_LIMIT
            || config.attempt_timeout.is_zero()
        {
            return Err(CatalogError::Storage("invalid publication bounds"));
        }
        database.entity::<intents::Entity>()?;
        Ok(Self {
            database,
            manager,
            config,
            after: None,
        })
    }

    /// Publish one bounded page. Pending intents are read in `(app, revision)`
    /// order; an app's first failure defers its later intents, and the cursor
    /// then moves to the next app so a blocked app cannot starve the others.
    ///
    /// # Errors
    /// Reports a failed page read. Individual failures leave their intents
    /// pending and are counted in the returned statistics.
    pub async fn tick(&mut self) -> Result<PublisherStats, CatalogError> {
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
            if blocked == Some(intent.app_id.as_str()) {
                stats.deferred += 1;
                continue;
            }
            stats.attempted += 1;
            let outcome = compio::time::timeout(self.config.attempt_timeout, self.publish(intent))
                .await
                .unwrap_or(Err(PublishError::Timeout));
            match outcome {
                Ok(()) => stats.acknowledged += 1,
                Err(error) => {
                    stats.failed += 1;
                    blocked = Some(intent.app_id.as_str());
                    tracing::warn!(app_id = %intent.app_id, revision = intent.revision,
                        action = %intent.action, %error, "lifecycle intent remains pending");
                }
            }
        }
        let full = usize::try_from(self.config.batch_size)
            .map_err(|_| CatalogError::Storage("invalid publication bounds"))?;
        self.after = if page.len() < full {
            None
        } else {
            page.last().map(|intent| intent.app_id.clone())
        };
        Ok(stats)
    }

    async fn publish(&self, intent: &PendingIntent) -> Result<(), PublishError> {
        let app = AppId::parse(&intent.app_id).map_err(|_| PublishError::InvalidIntent("app"))?;
        let revision =
            Revision::try_from(intent.revision).map_err(|_| PublishError::InvalidIntent("revision"))?;
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
    if updated > 1 || stored.state != ACKNOWLEDGED || !same_receipt(&intent.action, recorded, receipt)
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

/// Run the publisher for the life of the process. Without Control's service
/// signer the manager routes are unreachable, and intents stay pending.
pub async fn run(state: Arc<AppState>, coordinator_url: String, tick: Duration) {
    if state.service_auth.signing_identity().is_none() {
        tracing::warn!("control has no service signer; lifecycle intents remain pending");
        return;
    }
    let manager = match ControlCoordinator::new(
        &coordinator_url,
        state.service_auth.clone(),
        Options::default(),
    ) {
        Ok(manager) => manager,
        Err(error) => {
            tracing::error!(%error, "lifecycle publisher cannot reach the workflow manager");
            return;
        }
    };
    let mut publisher = None;
    loop {
        if publisher.is_none() {
            match catalog::connect(state.registry.workflow_store_db_url()).await {
                Ok(database) => {
                    match Publisher::new(database, manager.clone(), PublisherConfig::default()) {
                        Ok(connected) => publisher = Some(connected),
                        Err(error) => tracing::error!(%error, "lifecycle publisher is invalid"),
                    }
                }
                Err(error) => tracing::error!(%error, "lifecycle publisher could not connect"),
            }
        }
        if let Some(publisher) = &mut publisher {
            match publisher.tick().await {
                Ok(visited) if visited.attempted != 0 => {
                    tracing::info!(?visited, "lifecycle publisher visited pending intents");
                }
                Ok(_) => {}
                Err(error) => tracing::error!(%error, "lifecycle publication pass failed"),
            }
        }
        compio::time::sleep(tick).await;
    }
}
