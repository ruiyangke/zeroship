//! Platform deployment collection through durable queue and journal hold fences.
#![expect(
    clippy::future_not_send,
    reason = "Control ORM and blob I/O use compio"
)]

use crate::{AppState, config::ControlSettingsConsumer};
use chrono::Utc;
use std::{future::Future, sync::Arc, time::Duration};
use zeroship_bundle::BlobStore;
use zeroship_core::{app_id::AppId, config::DeclaredEnvKey, schema_name::SchemaName};
use zeroship_data_orm::{
    ConnectOptions,
    binding::DbBinding,
    encryption::ProjectKeySource,
    error::DbError,
    orm::{Database, FromRow},
    schema::Schema,
};
use zeroship_workflow_manager::deployments::{self, Error, models::app_deploys as deploys};

pub const DEFAULT_TICK_SECS: u64 = 60 * 60;
pub const DEFAULT_BATCH_SIZE: i64 = 128;
pub const DEFAULT_GRACE_WINDOW_MS: i64 = 0;
pub const DEFAULT_ATTEMPT_TIMEOUT: Duration = Duration::from_secs(10);
pub const GRACE_WINDOW_ENV: DeclaredEnvKey<String, ControlSettingsConsumer> =
    DeclaredEnvKey::platform("CONTROL_DEPLOY_RETENTION_GRACE_WINDOW_MS");
pub const BATCH_SIZE_ENV: DeclaredEnvKey<String, ControlSettingsConsumer> =
    DeclaredEnvKey::platform("CONTROL_DEPLOY_RETENTION_BATCH_SIZE");

#[derive(Debug, Clone, Copy)]
pub struct DeployRetentionConfig {
    pub grace_window_ms: i64,
    pub batch_size: i64,
    pub attempt_timeout: Duration,
}
impl Default for DeployRetentionConfig {
    fn default() -> Self {
        let grace = zeroship_core::read_declared_env!(GRACE_WINDOW_ENV, ControlSettingsConsumer)
            .ok()
            .flatten();
        let batch = zeroship_core::read_declared_env!(BATCH_SIZE_ENV, ControlSettingsConsumer)
            .ok()
            .flatten();
        Self {
            grace_window_ms: configured_integer(GRACE_WINDOW_ENV.name(), grace, 0)
                .unwrap_or(DEFAULT_GRACE_WINDOW_MS),
            batch_size: configured_integer(BATCH_SIZE_ENV.name(), batch, 1)
                .unwrap_or(DEFAULT_BATCH_SIZE),
            attempt_timeout: DEFAULT_ATTEMPT_TIMEOUT,
        }
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

// Only platform fields needed to serialize with normal activation.
zeroship_data_orm::orm::schema! {
    catalog {
        apps {
            #[orm(primary_key)]
            id: Text,
            deploy_hash: Nullable<Text>,
            env_version: BigInt,
        }
    }
}
use catalog::apps;

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
    activated_at: Option<i64>,
}

/// A bounded rotating catalog scan. Durable hold state supplies deletion
/// authority; restarting the scan resumes interrupted reclamation fences.
#[derive(Debug)]
pub struct Collector {
    database: Database,
    blobs: Arc<dyn BlobStore>,
    config: DeployRetentionConfig,
    after: Option<String>,
    upper: Option<String>,
}
impl Collector {
    /// Bind Control's platform database and normal manifest store.
    ///
    /// # Errors
    /// Rejects invalid bounds, incompatible metadata and unavailable storage.
    pub async fn connect(
        url: &str,
        blobs: Arc<dyn BlobStore>,
        config: DeployRetentionConfig,
    ) -> Result<Self, Error> {
        if config.grace_window_ms < 0
            || config.batch_size <= 0
            || config.batch_size > zeroship_data_orm::sql::MAX_ROW_LIMIT
            || config.attempt_timeout.is_zero()
            || std::time::Instant::now()
                .checked_add(config.attempt_timeout)
                .is_none()
        {
            return Err(Error::InvalidRequest(
                "invalid deployment collection bounds".into(),
            ));
        }
        let mut collections = deployments::collections()?.into_collections();
        collections.extend(catalog::schema().into_collections());
        let database = compio::time::timeout(
            config.attempt_timeout,
            Database::connect(
                DbBinding::new(
                    "platform",
                    "control-deployment-collector",
                    SchemaName::new("zeroship").map_err(|_| invalid_storage())?,
                ),
                ConnectOptions::new(url.to_owned(), ProjectKeySource::unavailable())
                    .connection_authority(),
                Schema::new(collections),
            ),
        )
        .await
        .map_err(|_| Error::Timeout)??;
        Ok(Self {
            database,
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
            let newest = compio::time::timeout(self.config.attempt_timeout, async {
                self.database
                    .entity::<deploys::Entity>()?
                    .query()
                    .filter(deploys::retention_state.ne("deleted")?)
                    .order_by(deploys::id.desc())
                    .first::<Candidate>()
                    .await
            })
            .await
            .map_err(|_| Error::Timeout)??;
            self.upper = newest.map(|row| row.id);
        }
        let Some(upper) = &self.upper else {
            return Ok(DeployRetentionStats::default());
        };
        let cutoff = Utc::now()
            .timestamp_millis()
            .checked_sub(self.config.grace_window_ms)
            .ok_or_else(|| Error::InvalidRequest("deployment grace is out of range".into()))?;
        let mut filter = deploys::retention_state.ne("deleted")?.and(
            deploys::retention_state
                .ne("available")?
                .or(deploys::activated_at
                    .ne(None::<i64>)?
                    .and(deploys::activated_at.lte(Some(cutoff))?)),
        );
        if let Some(after) = &self.after {
            filter = filter.and(deploys::id.gt(after.as_str())?);
        }
        filter = filter.and(deploys::id.lte(upper.as_str())?);
        let page = compio::time::timeout(self.config.attempt_timeout, async {
            self.database
                .entity::<deploys::Entity>()?
                .query()
                .filter(filter)
                .order_by(deploys::id.asc())
                .limit(self.config.batch_size)?
                .all::<Candidate>()
                .await
        })
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

    async fn collect(&self, candidate: &Candidate, cutoff: i64) -> Result<Option<bool>, Error> {
        let app = AppId::parse(&candidate.app_id).map_err(|_| invalid_storage())?;
        let hash = transact(&self.database, async |tx| {
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
            let deployment = tx
                .entity::<deploys::Entity>()?
                .query()
                .filter(
                    deploys::app_id
                        .eq(app.as_str())?
                        .and(deploys::id.eq(candidate.id.as_str())?),
                )
                .first::<Deployment>()
                .await?
                .ok_or_else(invalid_storage)?;
            if current.deploy_hash.as_deref() == Some(deployment.deploy_hash.as_str()) {
                return Ok(None);
            }
            match deployment.retention_state.as_str() {
                "deleted" => return Ok(None),
                "reclaiming" => {}
                "available" => {
                    if deployment.activated_at.is_none_or(|at| at > cutoff) {
                        return Ok(None);
                    }
                    let newest = tx
                        .entity::<deploys::Entity>()?
                        .query()
                        .filter(
                            deploys::app_id
                                .eq(app.as_str())?
                                .and(deploys::activated_at.ne(None::<i64>)?),
                        )
                        .order_by(deploys::activated_at.desc())
                        .order_by(deploys::created_at.desc())
                        .order_by(deploys::id.desc())
                        .first::<Deployment>()
                        .await?
                        .ok_or_else(invalid_storage)?;
                    if newest.id == deployment.id {
                        return Ok(None);
                    }
                }
                _ => return Err(invalid_storage()),
            }
            match deployments::fence_reclamation(&tx, &app, &candidate.id).await {
                Ok(hash) if hash == deployment.deploy_hash => Ok(Some(hash)),
                Ok(_) => Err(invalid_storage()),
                Err(Error::Conflict(_)) => Ok(None),
                Err(error) => Err(error),
            }
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
        deployments::finish_reclamation(&self.database, &app, &candidate.id).await?;
        Ok(Some(deleted))
    }
}

pub async fn run(state: Arc<AppState>, tick_secs: u64) {
    let config = DeployRetentionConfig::default();
    let mut collector = None;
    loop {
        if collector.is_none() {
            match Collector::connect(
                state.registry.workflow_store_db_url(),
                state.blob_store.clone(),
                config,
            )
            .await
            {
                Ok(connected) => collector = Some(connected),
                Err(error) => tracing::error!(%error, "deployment collector could not connect"),
            }
        }
        if let Some(collector) = &mut collector {
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
fn configured_integer(name: &str, raw: Option<String>, minimum: i64) -> Option<i64> {
    let raw = raw?;
    match raw.parse::<i64>() {
        Ok(value) if value >= minimum => Some(value),
        _ => {
            tracing::warn!(env = name, "ignoring invalid deployment collection bound");
            None
        }
    }
}
