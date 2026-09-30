//! Platform deployment collection through durable queue and journal hold fences.
#![expect(
    clippy::future_not_send,
    reason = "Control ORM and blob I/O use compio"
)]

use crate::publication::{
    catalog::{ACTIVATE, PENDING},
    models::catalog::{app_lifecycle_intents as intents, apps},
    Catalog,
};
use crate::config::ControlSettingsConsumer;
use chrono::Utc;
use std::{future::Future, sync::Arc, time::Duration};
use zeroship_bundle::BlobStore;
use zeroship_core::{app_id::AppId, config::DeclaredEnvKey};
use zeroship_data_orm::{
    error::DbError,
    orm::{Database, FromRow, UtcInstant},
};
use zeroship_workflow_manager::deployments::{self, Error, models::app_deploys as deploys};

pub const DEFAULT_TICK_SECS: u64 = 60 * 60;
pub const DEFAULT_BATCH_SIZE: i64 = 128;
pub const DEFAULT_GRACE_WINDOW_MS: i64 = 0;
pub const DEFAULT_ATTEMPT_TIMEOUT: Duration = Duration::from_secs(10);
pub const GRACE_WINDOW_ENV: DeclaredEnvKey<i64, ControlSettingsConsumer> =
    DeclaredEnvKey::platform("CONTROL_DEPLOY_RETENTION_GRACE_WINDOW_MS");
pub const BATCH_SIZE_ENV: DeclaredEnvKey<i64, ControlSettingsConsumer> =
    DeclaredEnvKey::platform("CONTROL_DEPLOY_RETENTION_BATCH_SIZE");

#[derive(Debug, Clone, Copy)]
pub struct DeployRetentionConfig {
    pub grace_window_ms: i64,
    pub batch_size: i64,
    pub attempt_timeout: Duration,
}
impl Default for DeployRetentionConfig {
    fn default() -> Self {
        Self {
            grace_window_ms: DEFAULT_GRACE_WINDOW_MS,
            batch_size: DEFAULT_BATCH_SIZE,
            attempt_timeout: DEFAULT_ATTEMPT_TIMEOUT,
        }
    }
}

impl DeployRetentionConfig {
    /// The defaults, with each bound the operator configured in its place.
    ///
    /// A bound that does not parse, or that collection cannot honour, is
    /// refused rather than replaced: the process refuses to start, instead of
    /// running without the collection the operator configured.
    ///
    /// # Errors
    /// Names the first configured bound that is unusable.
    pub fn configured() -> Result<Self, String> {
        let grace = zeroship_core::read_declared_env!(GRACE_WINDOW_ENV, ControlSettingsConsumer)
            .map_err(|error| error.to_string())?;
        let batch = zeroship_core::read_declared_env!(BATCH_SIZE_ENV, ControlSettingsConsumer)
            .map_err(|error| error.to_string())?;
        let config = Self {
            grace_window_ms: grace.unwrap_or(DEFAULT_GRACE_WINDOW_MS),
            batch_size: batch.unwrap_or(DEFAULT_BATCH_SIZE),
            ..Self::default()
        };
        config.validate()?;
        Ok(config)
    }

    /// Check that collection can honour every bound.
    ///
    /// # Errors
    /// Names the first bound it cannot.
    pub fn validate(&self) -> Result<(), String> {
        let cutoff = Utc::now()
            .timestamp_millis()
            .checked_sub(self.grace_window_ms)
            .and_then(|millis| UtcInstant::from_unix_millis(millis).ok());
        if self.grace_window_ms < 0 || cutoff.is_none() {
            return Err(format!(
                "{} must be a non-negative number of milliseconds that leaves a cutoff in the \
                 calendar",
                GRACE_WINDOW_ENV.name()
            ));
        }
        if self.batch_size <= 0 || self.batch_size > zeroship_data_orm::sql::MAX_ROW_LIMIT {
            return Err(format!(
                "{} must be between 1 and {}",
                BATCH_SIZE_ENV.name(),
                zeroship_data_orm::sql::MAX_ROW_LIMIT
            ));
        }
        if self.attempt_timeout.is_zero()
            || std::time::Instant::now()
                .checked_add(self.attempt_timeout)
                .is_none()
        {
            return Err(
                "the deployment collection attempt timeout must be positive and in range".into(),
            );
        }
        Ok(())
    }
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct DeployRetentionStats {
    pub candidates: usize,
    pub retained: usize,
    pub manifests_deleted: usize,
    pub finished: usize,
    pub failed: usize,
}

#[derive(FromRow)]
#[orm(entity = apps)]
struct CurrentApp {
    deploy_hash: Option<String>,
    env_version: i64,
}
#[derive(FromRow)]
#[orm(entity = deploys)]
struct Candidate {
    id: String,
    app_id: String,
}
#[derive(FromRow)]
#[orm(entity = deploys)]
struct Deployment {
    id: String,
    deploy_hash: String,
    retention_state: String,
    activated_at: Option<UtcInstant>,
}

/// A bounded rotating catalog scan. Durable hold state supplies deletion
/// authority; restarting the scan resumes interrupted reclamation fences.
///
/// Its catalog reads and transactions run on the retention executor, one
/// operation at a time; manifest deletion runs on the caller's thread, between
/// them, so no lane waits on blob I/O.
#[derive(Debug)]
pub struct Collector {
    executor: Catalog,
    blobs: Arc<dyn BlobStore>,
    config: DeployRetentionConfig,
    after: Option<String>,
    upper: Option<String>,
}
impl Collector {
    /// Collect through Control's retention executor and normal manifest store.
    ///
    /// # Errors
    /// Rejects bounds [`DeployRetentionConfig::validate`] refuses.
    pub fn new(
        executor: Catalog,
        blobs: Arc<dyn BlobStore>,
        config: DeployRetentionConfig,
    ) -> Result<Self, Error> {
        config.validate().map_err(Error::InvalidRequest)?;
        Ok(Self {
            executor,
            blobs,
            config,
            after: None,
            upper: None,
        })
    }

    /// Visit a bounded page, including interrupted deletions. Retained and failed
    /// candidates advance the cursor so another app can make progress.
    ///
    /// # Errors
    /// Reports page-read failures. Individual failures preserve their fences and
    /// are reported in the returned statistics and structured logs.
    pub async fn tick(&mut self) -> Result<DeployRetentionStats, Error> {
        if self.upper.is_none() {
            let newest = compio::time::timeout(
                self.config.attempt_timeout,
                self.executor.run(|database| async move {
                    let newest = database
                        .entity::<deploys::Entity>()?
                        .query()
                        .filter(deploys::retention_state.ne("deleted")?)
                        .order_by(deploys::id.desc())
                        .first::<Candidate>()
                        .await?;
                    Ok::<_, Error>(newest.map(|row| row.id))
                }),
            )
            .await
            .map_err(|_| Error::Timeout)??;
            self.upper = newest;
        }
        let Some(upper) = self.upper.clone() else {
            return Ok(DeployRetentionStats::default());
        };
        let cutoff = Utc::now()
            .timestamp_millis()
            .checked_sub(self.config.grace_window_ms)
            .ok_or(())
            .and_then(|millis| UtcInstant::from_unix_millis(millis).map_err(|_| ()))
            .map_err(|()| Error::InvalidRequest("deployment grace is out of range".into()))?;
        let (after, batch_size) = (self.after.clone(), self.config.batch_size);
        let page = compio::time::timeout(
            self.config.attempt_timeout,
            self.executor.run(move |database| async move {
                let mut filter = deploys::retention_state.ne("deleted")?.and(
                    deploys::retention_state
                        .ne("available")?
                        .or(deploys::activated_at
                            .ne(None::<UtcInstant>)?
                            .and(deploys::activated_at.lte(Some(cutoff))?)),
                );
                if let Some(after) = &after {
                    filter = filter.and(deploys::id.gt(after.as_str())?);
                }
                filter = filter.and(deploys::id.lte(upper.as_str())?);
                let page = database
                    .entity::<deploys::Entity>()?
                    .query()
                    .filter(filter)
                    .order_by(deploys::id.asc())
                    .limit(batch_size)?
                    .all::<Candidate>()
                    .await?;
                Ok::<_, Error>(page)
            }),
        )
        .await
        .map_err(|_| Error::Timeout)??;
        let mut stats = DeployRetentionStats::default();
        for candidate in &page {
            self.after = Some(candidate.id.clone());
            stats.candidates += 1;
            let result =
                compio::time::timeout(self.config.attempt_timeout, self.collect(candidate, cutoff))
                    .await
                    .unwrap_or(Err(Error::Timeout));
            match result {
                Ok(Some(deleted)) => {
                    stats.finished += 1;
                    stats.manifests_deleted += usize::from(deleted);
                }
                Ok(None) => stats.retained += 1,
                Err(error) => {
                    stats.failed += 1;
                    tracing::warn!(app_id = %candidate.app_id, deploy_id = %candidate.id,
                        %error, "deployment collection will retry");
                }
            }
        }
        if page.len() < usize::try_from(self.config.batch_size).map_err(|_| invalid_storage())? {
            self.after = None;
            self.upper = None;
        }
        Ok(stats)
    }

    async fn collect(
        &self,
        candidate: &Candidate,
        cutoff: UtcInstant,
    ) -> Result<Option<bool>, Error> {
        let app = AppId::parse(&candidate.app_id).map_err(|_| invalid_storage())?;
        let (fenced, deployment) = (app.clone(), candidate.id.clone());
        let hash = self
            .executor
            .run(move |database| async move {
                fence(&database, &fenced, &deployment, cutoff).await
            })
            .await?;
        let Some(hash) = hash else {
            return Ok(None);
        };
        // External deletion only follows a known committed admission fence.
        let deleted = self
            .blobs
            .delete_manifest(&app, &hash)
            .await
            .map_err(|error| Error::Unavailable(error.to_string()))?;
        let deployment = candidate.id.clone();
        self.executor
            .run(move |database| async move {
                deployments::finish_reclamation(&database, &app, &deployment).await
            })
            .await?;
        Ok(Some(deleted))
    }
}

/// Fence `deployment` for reclamation in one transaction when nothing retains
/// it any more, returning the hash whose manifest may then be deleted.
async fn fence(
    database: &Database,
    app: &AppId,
    deployment: &str,
    cutoff: UtcInstant,
) -> Result<Option<String>, Error> {
    transact(database, async |tx| {
        // Match Registry's apps -> app_deploys lock order. The guarded no-op
        // set cannot overwrite a concurrently changed counter.
        let Some(observed) = tx
            .entity::<apps::Entity>()?
            .query()
            .filter(apps::id.eq(app.as_str())?)
            .first::<CurrentApp>()
            .await?
        else {
            return Ok(None);
        };
        if tx
            .entity::<apps::Entity>()?
            .update_many(
                apps::id
                    .eq(app.as_str())?
                    .and(apps::env_version.eq(observed.env_version)?),
                apps::env_version.set(observed.env_version)?,
            )
            .await?
            != 1
        {
            return Ok(None);
        }
        let current = tx
            .entity::<apps::Entity>()?
            .query()
            .filter(apps::id.eq(app.as_str())?)
            .first::<CurrentApp>()
            .await?
            .ok_or_else(invalid_storage)?;
        if current
            .deploy_hash
            .as_deref()
            .is_some_and(|hash| !zeroship_bundle::validate_hash_format(hash))
        {
            return Err(invalid_storage());
        }
        let found = tx
            .entity::<deploys::Entity>()?
            .query()
            .filter(
                deploys::app_id
                    .eq(app.as_str())?
                    .and(deploys::id.eq(deployment)?),
            )
            .first::<Deployment>()
            .await?
            .ok_or_else(invalid_storage)?;
        if current.deploy_hash.as_deref() == Some(found.deploy_hash.as_str()) {
            return Ok(None);
        }
        // An activation the manager has not acknowledged still needs this
        // bundle: the manager acquires its queue hold only when it accepts
        // the activation. Deploy and restore insert the intent under this
        // same app lock, and acknowledging it is the update that removes
        // this dependency once the queue hold exists.
        let pending_activation = tx
            .entity::<intents::Entity>()?
            .query()
            .filter(
                intents::app_id
                    .eq(app.as_str())?
                    .and(intents::deploy_id.eq(Some(deployment))?)
                    .and(intents::action.eq(ACTIVATE)?)
                    .and(intents::state.eq(PENDING)?),
            )
            .exists()
            .await?;
        match found.retention_state.as_str() {
            "deleted" => return Ok(None),
            // Activation refuses reclaiming storage, so a pending intent
            // cannot legitimately name a fenced deployment.
            "reclaiming" if pending_activation => return Err(invalid_storage()),
            "available" if pending_activation => return Ok(None),
            "reclaiming" => {}
            "available" => {
                if found.activated_at.is_none_or(|at| at > cutoff) {
                    return Ok(None);
                }
                if newest_activated(&tx, app).await?.id == found.id {
                    return Ok(None);
                }
            }
            _ => return Err(invalid_storage()),
        }
        match deployments::fence_reclamation(&tx, app, deployment).await {
            Ok(hash) if hash == found.deploy_hash => Ok(Some(hash)),
            Ok(_) => Err(invalid_storage()),
            Err(Error::Conflict(_)) => Ok(None),
            Err(error) => Err(error),
        }
    })
    .await
}

/// The app's most recently activated deployment.
async fn newest_activated(tx: &Database, app: &AppId) -> Result<Deployment, Error> {
    tx.entity::<deploys::Entity>()?
        .query()
        .filter(
            deploys::app_id
                .eq(app.as_str())?
                .and(deploys::activated_at.ne(None::<UtcInstant>)?),
        )
        .order_by(deploys::activated_at.desc())
        .order_by(deploys::created_at.desc())
        .order_by(deploys::id.desc())
        .first::<Deployment>()
        .await?
        .ok_or_else(invalid_storage)
}

/// Visit a page every `tick_secs` for the life of the process.
pub async fn run(mut collector: Collector, tick_secs: u64) {
    loop {
        match collector.tick().await {
            Ok(progress) if progress.candidates != 0 => tracing::info!(
                candidates = progress.candidates,
                retained = progress.retained,
                manifests_deleted = progress.manifests_deleted,
                finished = progress.finished,
                failed = progress.failed,
                "deployment collector visited catalog page"
            ),
            Ok(_) => {}
            Err(error) => tracing::error!(%error, "deployment collection tick failed"),
        }
        compio::time::sleep(Duration::from_secs(tick_secs)).await;
    }
}

fn invalid_storage() -> Error {
    Error::Internal("invalid deployment catalog metadata".into())
}

async fn transact<T, F, Fut>(database: &Database, body: F) -> Result<T, Error>
where
    F: FnOnce(Database) -> Fut,
    Fut: Future<Output = Result<T, Error>>,
{
    const FAILED: &str = "deployment_collector_refused";
    let mut original = None;
    let saved = &mut original;
    let result = database
        .transaction(|tx| {
            let callback: std::pin::Pin<Box<dyn Future<Output = Result<T, DbError>> + '_>> =
                Box::pin(async move {
                    match body(tx).await {
                        Ok(value) => Ok(value),
                        Err(error) => {
                            *saved = Some(error);
                            Err(DbError::validation(FAILED, "deployment collection refused"))
                        }
                    }
                });
            callback
        })
        .await;
    match result {
        Ok(value) => Ok(value),
        Err(DbError::ValidationFailed { code: FAILED, .. }) => {
            Err(original.unwrap_or_else(invalid_storage))
        }
        Err(error) => Err(error.into()),
    }
}
