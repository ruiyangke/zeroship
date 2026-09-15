//! Serialize authoritative observations before reading their contributing inputs.

use super::{models::schema, PolicyObservation};
use crate::Error;
use schema::{apps, plans, workflow_policy_ledger as ledger, workflow_rollout_config as rollout};
use std::time::{Duration, Instant};
use zeroship_core::{app_id::AppId, workflow_coordination::Revision, workflow_policy::AppPolicy};
use zeroship_data_orm::{
    error::DbError,
    orm::{
        ConflictTarget, Database, FromRow, Insertable, IsolationLevel, TransactionOptions,
        UtcInstant,
    },
    Value,
};

/// Complete operator-owned source settings, independent of creator input.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RolloutPolicy {
    pub dispatch_paused: bool,
    pub ingress_disabled: bool,
    pub source_validity_ms: i64,
}

/// Control-schema storage only. It does not open or inspect creator databases.
#[derive(Clone, Debug)]
pub struct ControlPolicyStore {
    database: Database,
}

impl ControlPolicyStore {
    /// Bind the host's provisioned Control schema. Observations require explicit
    /// read-committed isolation; an unsupported backend refuses the operation.
    ///
    /// # Errors
    /// Rejects missing or incompatible native metadata.
    pub fn new(database: Database) -> Result<Self, Error> {
        database.entity::<apps::Entity>()?;
        database.entity::<plans::Entity>()?;
        database.entity::<rollout::Entity>()?;
        database.entity::<ledger::Entity>()?;
        Ok(Self { database })
    }

    /// Verify the provisioned columns and service read permissions. An empty
    /// operator catalog is allowed; individual unconfigured apps remain unavailable.
    ///
    /// # Errors
    /// Refuses unavailable or incompatible source storage.
    pub async fn ready(&self) -> Result<(), Error> {
        async {
            read_source(&self.database, None).await?;
            let publication = self
                .database
                .entity::<ledger::Entity>()?
                .alias("publication")?;
            self.database
                .from(&publication)
                .select((
                    publication.column(ledger::id).select::<String>(),
                    publication.row::<LedgerRecord>(),
                ))?
                .limit(1)?
                .all()
                .await?;
            Ok::<_, DbError>(())
        }
        .await
        .map_err(|_| Error::Unavailable)
    }

    /// Read every contributor in a statement snapshot after acquiring the
    /// publication row. Only committed observations may become source authority.
    ///
    /// # Errors
    /// Missing, malformed, expired or unavailable source state returns Unavailable.
    pub async fn observe(&self, app: &AppId) -> Result<PolicyObservation, Error> {
        let started = Instant::now();
        let (revision, policy, expires_at) = self
            .database
            .transaction_with_options(
                TransactionOptions::default().isolation_level(IsolationLevel::ReadCommitted),
                |tx| async move { publish(&tx, app, started).await },
            )
            .await
            .map_err(|_| Error::Unavailable)?;
        PolicyObservation::new(app.clone(), revision, policy, expires_at)
    }

    /// Set complete plan authority without changing plan eligibility or pricing.
    /// The calling platform host authenticates its operator before using this API.
    ///
    /// # Errors
    /// Invalid policy is rejected before I/O; missing plans and failed writes are unavailable.
    pub async fn set_plan_policy(&self, plan: &str, policy: &AppPolicy) -> Result<(), Error> {
        policy.validate().map_err(|_| Error::Invalid)?;
        let json = encode(policy).map_err(|_| Error::Invalid)?;
        let changed = self
            .database
            .entity::<plans::Entity>()?
            .update_many(
                plans::id.eq(plan)?,
                plans::workflow_policy_json.set(Some(json))?,
            )
            .await
            .map_err(|_| Error::Unavailable)?;
        if changed != 1 {
            return Err(Error::Unavailable);
        }
        Ok(())
    }

    /// Publish operator switches and their finite source validity together.
    /// Existing observations retain their original deadline until refresh.
    ///
    /// # Errors
    /// Invalid validity is rejected before I/O; failed writes are unavailable.
    pub async fn set_rollout(&self, settings: RolloutPolicy) -> Result<(), Error> {
        validity(settings.source_validity_ms).map_err(|_| Error::Invalid)?;
        let _: RolloutRecord = self
            .database
            .entity::<rollout::Entity>()?
            .upsert(
                NewRollout {
                    id: "global".into(),
                    dispatch_paused: settings.dispatch_paused,
                    ingress_disabled: settings.ingress_disabled,
                    source_validity_ms: settings.source_validity_ms,
                },
                ConflictTarget::new(rollout::id),
            )
            .await
            .map_err(|_| Error::Unavailable)?;
        Ok(())
    }
}

#[derive(FromRow)]
#[orm(entity = apps)]
struct AppRecord {
    workflows_enabled: bool,
    archived_at: Option<UtcInstant>,
}
#[derive(FromRow)]
#[orm(entity = plans)]
struct PlanRecord {
    workflows_allowed: bool,
    archived: bool,
    workflow_policy_json: Option<Value>,
}
#[derive(FromRow)]
#[orm(entity = rollout)]
struct RolloutRecord {
    dispatch_paused: bool,
    ingress_disabled: bool,
    source_validity_ms: i64,
}
#[derive(Insertable)]
#[orm(entity = rollout)]
struct NewRollout {
    id: String,
    dispatch_paused: bool,
    ingress_disabled: bool,
    source_validity_ms: i64,
}
#[derive(FromRow)]
#[orm(entity = ledger)]
struct LedgerRecord {
    revision: i64,
    policy_json: Option<Value>,
    source_validity_ms: Option<i64>,
}
#[derive(Insertable)]
#[orm(entity = ledger)]
struct NewLedger {
    id: String,
}

async fn read_source(
    tx: &Database,
    app: Option<&AppId>,
) -> Result<Vec<(AppRecord, PlanRecord, RolloutRecord)>, DbError> {
    let app_source = tx.entity::<apps::Entity>()?.alias("app")?;
    let plan_source = tx.entity::<plans::Entity>()?.alias("plan")?;
    let switches = tx.entity::<rollout::Entity>()?.alias("switches")?;
    let filter = match app {
        Some(app) => app_source.column(apps::id).eq(app.as_str())?,
        // Readiness must check the selector's grant even without a requested app.
        None => app_source.column(apps::id).is_not_null(),
    };
    let query = tx
        .from(&app_source)
        .inner_join(
            &plan_source,
            app_source
                .column(apps::plan_id)
                .eq(plan_source.column(plans::id))?,
        )?
        .inner_join(&switches, switches.column(rollout::id).eq("global")?)?
        .filter(filter)
        .select((
            app_source.row::<AppRecord>(),
            plan_source.row::<PlanRecord>(),
            switches.row::<RolloutRecord>(),
        ))?;
    let query = if app.is_none() {
        query.limit(1)?
    } else {
        query
    };
    query.all().await
}

async fn publish(
    tx: &Database,
    app: &AppId,
    started: Instant,
) -> Result<(Revision, AppPolicy, Instant), DbError> {
    // An ID-only upsert leaves existing publication fields unchanged while
    // holding its write lock. Inputs are read only after that wait completes.
    let previous: LedgerRecord = tx
        .entity::<ledger::Entity>()?
        .upsert(
            NewLedger {
                id: app.as_str().into(),
            },
            ConflictTarget::new(ledger::id),
        )
        .await?;
    let rows = read_source(tx, Some(app)).await?;
    let [(app_record, plan, switches)] = rows.as_slice() else {
        return Err(unavailable());
    };
    let mut policy = decode(plan.workflow_policy_json.as_ref().ok_or_else(unavailable)?)?;
    let enabled = app_record.workflows_enabled
        && app_record.archived_at.is_none()
        && plan.workflows_allowed
        && !plan.archived;
    policy.admission &= enabled;
    policy.dispatch &= enabled && !switches.dispatch_paused;
    policy.ingress &= enabled && !switches.ingress_disabled;
    let expires_at = started
        .checked_add(validity(switches.source_validity_ms)?)
        .filter(|deadline| *deadline > Instant::now())
        .ok_or_else(unavailable)?;
    let unchanged = match (
        previous.revision,
        previous.policy_json.as_ref(),
        previous.source_validity_ms,
    ) {
        (0, None, None) => false,
        (revision, Some(stored), Some(source_validity_ms)) if revision > 0 => {
            validity(source_validity_ms)?;
            decode(stored)? == policy && source_validity_ms == switches.source_validity_ms
        }
        _ => return Err(unavailable()),
    };
    let revision = if unchanged {
        previous.revision
    } else {
        let next = previous.revision.checked_add(1).ok_or_else(unavailable)?;
        let changes = ledger::revision
            .set(next)?
            .and(ledger::policy_json.set(Some(encode(&policy)?))?)?
            .and(ledger::source_validity_ms.set(Some(switches.source_validity_ms))?)?;
        if tx
            .entity::<ledger::Entity>()?
            .update_many(ledger::id.eq(app.as_str())?, changes)
            .await?
            != 1
        {
            return Err(unavailable());
        }
        next
    };
    if expires_at <= Instant::now() {
        return Err(unavailable());
    }
    Ok((
        Revision::try_from(revision).map_err(|_| unavailable())?,
        policy,
        expires_at,
    ))
}

fn encode(policy: &AppPolicy) -> Result<Value, DbError> {
    serde_json::to_value(policy)
        .map(Into::into)
        .map_err(|_| unavailable())
}
fn decode(value: &Value) -> Result<AppPolicy, DbError> {
    let policy: AppPolicy =
        serde_json::from_value(serde_json::to_value(value).map_err(|_| unavailable())?)
            .map_err(|_| unavailable())?;
    policy.validate().map_err(|_| unavailable())?;
    Ok(policy)
}
fn validity(milliseconds: i64) -> Result<Duration, DbError> {
    let value = u64::try_from(milliseconds)
        .ok()
        .filter(|value| *value > 0)
        .ok_or_else(unavailable)?;
    let duration = Duration::from_millis(value);
    Instant::now()
        .checked_add(duration)
        .ok_or_else(unavailable)?;
    Ok(duration)
}
fn unavailable() -> DbError {
    DbError::internal("workflow policy source unavailable")
}
