use super::{
    app::{decode, encode, insert_root_run, live_runs, lock_app},
    models,
    store::Transaction,
    AppPolicy, DeployRegistration, WorkflowService,
};
use crate::{calendar::Calendar, operations::StartOptions, validation, WorkflowServiceError};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use zeroship_core::{app_id::AppId, typed_id};
use zeroship_data_orm::{
    orm::{Entity, FindOptions, Output},
    sql::{Predicate, RowLimit},
    value,
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ScheduleRegistration {
    pub name: String,
    pub workflow_name: String,
    pub schedule: ScheduleTiming,
    #[serde(default)]
    pub input: Value,
    #[serde(default)]
    pub overlap: ScheduleOverlap,
    #[serde(default)]
    pub catch_up: ScheduleCatchUp,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase", deny_unknown_fields)]
pub enum ScheduleTiming {
    Cron {
        cron_expr: String,
        tz: String,
    },
    Interval {
        interval_ms: i64,
        anchor: IntervalAnchor,
    },
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum IntervalAnchor {
    Epoch,
    Deploy,
}
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ScheduleOverlap {
    #[default]
    Allow,
    SkipIfRunning,
}
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "lowercase", deny_unknown_fields)]
pub enum ScheduleCatchUp {
    #[default]
    Skip,
    Backfill {
        max: usize,
    },
}

impl ScheduleRegistration {
    fn validate(
        &self,
        deploy: &DeployRegistration,
        policy: &AppPolicy,
        now: i64,
    ) -> Result<(), WorkflowServiceError> {
        validation::workflow_name(&self.name)?;
        validation::workflow_name(&self.workflow_name)?;
        if !deploy.workflows.contains(&self.workflow_name) {
            return Err(WorkflowServiceError::InvalidRequest(
                "scheduled workflow is absent from the deployment".into(),
            ));
        }
        if encode(&self.input)?.len() > policy.max_input_bytes {
            return Err(WorkflowServiceError::PayloadTooLarge);
        }
        if let ScheduleCatchUp::Backfill { max } = self.catch_up {
            if max == 0 || max > policy.max_schedule_backfill {
                return Err(WorkflowServiceError::InvalidRequest(
                    "schedule backfill exceeds the app limit".into(),
                ));
            }
        }
        if let ScheduleTiming::Interval { interval_ms, .. } = &self.schedule {
            if *interval_ms < policy.min_schedule_interval_ms {
                return Err(WorkflowServiceError::InvalidRequest(
                    "schedule interval is below the app limit".into(),
                ));
            }
        }
        self.schedule.next_after(now, now)?;
        Ok(())
    }
}
impl ScheduleTiming {
    pub fn next_after(&self, after: i64, activated_at: i64) -> Result<i64, WorkflowServiceError> {
        let overflow = || {
            WorkflowServiceError::InvalidRequest(
                "workflow schedule timestamp is out of range".into(),
            )
        };
        match self {
            Self::Cron { cron_expr, tz } => Calendar::parse(cron_expr, tz)?
                .next_after(DateTime::<Utc>::from_timestamp_millis(after).ok_or_else(overflow)?)
                .map(|time| time.timestamp_millis()),
            Self::Interval {
                interval_ms,
                anchor,
            } => {
                if *interval_ms <= 0 {
                    return Err(WorkflowServiceError::InvalidRequest(
                        "workflow interval must be positive".into(),
                    ));
                }
                let anchor = if *anchor == IntervalAnchor::Epoch {
                    0
                } else {
                    activated_at
                };
                let elapsed = after.checked_sub(anchor).ok_or_else(overflow)?;
                let periods = elapsed
                    .div_euclid(*interval_ms)
                    .checked_add(1)
                    .ok_or_else(overflow)?;
                let next = anchor
                    .checked_add(periods.checked_mul(*interval_ms).ok_or_else(overflow)?)
                    .ok_or_else(overflow)?;
                DateTime::<Utc>::from_timestamp_millis(next).ok_or_else(overflow)?;
                Ok(next)
            }
        }
    }
}

/// Deployment reconciliation shares activation's transaction. Re-delivery of
/// the active deployment preserves its due frontier; replacing a deployment starts
/// its schedule frontier at activation, without inventing earlier occurrences.
pub(crate) async fn reconcile(
    tx: &mut Transaction,
    app: &AppId,
    deploy: &DeployRegistration,
    policy: &AppPolicy,
    now: i64,
) -> Result<(), WorkflowServiceError> {
    if deploy.schedules.len() > policy.max_schedules {
        return Err(WorkflowServiceError::ResourceExhausted(
            "workflow schedule limit reached".into(),
        ));
    }
    let mut names = std::collections::BTreeSet::new();
    for registration in &deploy.schedules {
        registration.validate(deploy, policy, now)?;
        if !names.insert(&registration.name) {
            return Err(WorkflowServiceError::InvalidRequest(
                "workflow schedule names must be unique".into(),
            ));
        }
    }
    let db = tx.database();
    let schedules = db.collection(models::schedules::Entity::COLLECTION)?;
    let source = db.entity::<models::schedules::Entity>()?.alias("s")?;
    let page_limit = RowLimit::default().get();
    let mut after: Option<String> = None;
    let mut existing = std::collections::BTreeMap::new();
    loop {
        let mut filter = vec![source.column(models::schedules::app_id).eq(app.as_str())?];
        if let Some(after) = &after {
            filter.push(source.column(models::schedules::id).gt(after.as_str())?);
        }
        let page = db
            .from(&source)
            .filter(Predicate::And(filter))
            .order_by(source.column(models::schedules::id).asc())
            .select(source.row::<models::ScheduleRecord>())?
            .limit(page_limit)?
            .all()
            .await?;
        let count = page.len();
        for row in page {
            after = Some(row.id.clone());
            if names.contains(&row.name) {
                existing.insert(row.name.clone(), row);
            } else if row.next_at.is_some() {
                schedules
                    .update(
                        value!({"app_id":app.as_str(), "id":row.id}),
                        value!({"next_at":null}),
                    )
                    .await?;
            }
        }
        if count < page_limit as usize {
            break;
        }
    }
    for registration in &deploy.schedules {
        let encoded = encode(registration)?;
        let row = existing.get(&registration.name);
        if let Some(row) = row {
            if row.next_at.is_some()
                && row.deploy_id == deploy.id
                && decode::<ScheduleRegistration>(&row.definition)? == *registration
            {
                continue;
            }
        }
        let next = registration.schedule.next_after(now, now)?;
        if let Some(row) = row {
            let revision = row.revision.checked_add(1).ok_or_else(|| {
                WorkflowServiceError::Internal("workflow schedule revision overflow".into())
            })?;
            schedules.update(
                value!({"app_id":app.as_str(), "id":row.id.clone()}),
                value!({"workflow_name":registration.workflow_name.clone(), "deploy_id":deploy.id.clone(),
                    "definition":encoded, "next_at":next, "anchor_at":now, "revision":revision}),
            ).await?;
        } else {
            schedules.insert(value!({
                "app_id":app.as_str(), "id":typed_id::new_workflow_schedule_id(), "name":registration.name.clone(),
                "workflow_name":registration.workflow_name.clone(), "deploy_id":deploy.id.clone(),
                "definition":encoded, "next_at":next, "anchor_at":now, "revision":0, "last_checked_at":0,
            })).await?;
        }
    }
    Ok(())
}

impl WorkflowService {
    /// Invoked by the host maintenance loop; occurrences and runs commit together.
    pub async fn tick_schedules(&self) -> Result<usize, WorkflowServiceError> {
        let mut tx = self.begin().await?;
        let now = tx.now().await?;
        let schedules = tx.table("schedules");
        let deploys = tx.table("deploys");
        let (scope, app_ids) = tx.host_app_scope()?;
        let due=tx.query(&format!("SELECT s.app_id,s.id FROM {schedules} s WHERE s.app_id IN ({scope}) AND next_at <= $2 AND EXISTS (SELECT 1 FROM {deploys} d WHERE d.app_id=s.app_id AND d.id=s.deploy_id AND d.state='available') ORDER BY last_checked_at,next_at,s.app_id,s.id LIMIT 128"), &[app_ids,now.into()]).await?;
        tx.commit().await?;
        let mut fired = 0;
        for candidate in due {
            let app = AppId::parse(&candidate.text("app_id")?).map_err(|_| {
                WorkflowServiceError::Internal("invalid persisted workflow app identity".into())
            })?;
            let id = candidate.text("id")?;
            let mut tx = self.begin().await?;
            let policy = lock_app(&mut tx, &app).await?;
            let now = tx.now().await?;
            let schedules = tx
                .database()
                .collection(models::schedules::Entity::COLLECTION)?;
            // This write holds the schedule row lock through occurrence persistence.
            schedules
                .update(
                    value!({"app_id":app.as_str(), "id":id.clone()}),
                    value!({"last_checked_at":now}),
                )
                .await?;
            let rows = tx
                .database()
                .entity::<models::schedules::Entity>()?
                .find::<models::ScheduleRecord>(
                    models::schedules::app_id
                        .eq(app.as_str())?
                        .and(models::schedules::id.eq(id.as_str())?),
                    FindOptions {
                        limit: Some(1),
                        ..Default::default()
                    },
                )
                .await?;
            let Some(row) = rows.first() else {
                tx.commit().await?;
                continue;
            };
            let Some(mut at) = row.next_at.filter(|at| *at <= now) else {
                tx.commit().await?;
                continue;
            };
            if !policy.admission {
                tx.commit().await?;
                continue;
            }
            // Candidate discovery can race executable loss or schedule updates.
            // Keep the due frontier intact until its current deployment is ready.
            let available = tx
                .database()
                .entity::<models::deploys::Entity>()?
                .find::<models::DeploymentHash>(
                    models::deploys::app_id
                        .eq(app.as_str())?
                        .and(models::deploys::id.eq(row.deploy_id.as_str())?)
                        .and(models::deploys::state.eq("available")?),
                    FindOptions {
                        limit: Some(1),
                        ..Default::default()
                    },
                )
                .await?;
            if available.is_empty() {
                tx.commit().await?;
                continue;
            }
            let registration: ScheduleRegistration = decode(&row.definition)?;
            let anchor = row.anchor_at;
            let cap = match registration.catch_up {
                ScheduleCatchUp::Skip => 1,
                ScheduleCatchUp::Backfill { max } => max.min(policy.max_schedule_backfill),
            };
            let runs = tx.database().collection(models::runs::Entity::COLLECTION)?;
            let occurrences = tx
                .database()
                .collection(models::occurrences::Entity::COLLECTION)?;
            for index in 0..cap {
                let Output::Count(duplicate) = occurrences
                    .count(
                        value!({"app_id":app.as_str(), "schedule_id":id.clone(), "at":at}),
                        value!({}),
                    )
                    .await?
                else {
                    return Err(WorkflowServiceError::Internal(
                        "workflow occurrence count returned rows".into(),
                    ));
                };
                if duplicate == 0 {
                    let skip = if registration.overlap == ScheduleOverlap::SkipIfRunning {
                        let source = tx.database().entity::<models::runs::Entity>()?.alias("r")?;
                        !tx.database()
                            .from(&source)
                            .filter(Predicate::And(vec![
                                source.column(models::runs::app_id).eq(app.as_str())?,
                                source
                                    .column(models::runs::schedule_id)
                                    .eq(Some(id.as_str()))?,
                                Predicate::Not(Box::new(Predicate::Or(vec![
                                    source.column(models::runs::state).eq("completed")?,
                                    source.column(models::runs::state).eq("failed")?,
                                    source.column(models::runs::state).eq("cancelled")?,
                                ]))),
                            ]))
                            .select(source.row::<models::KeyedRun>())?
                            .limit(1)?
                            .all()
                            .await?
                            .is_empty()
                    } else {
                        false
                    };
                    let run_id = if skip {
                        None
                    } else {
                        if live_runs(&tx, &app).await? >= policy.max_live_runs {
                            break;
                        }
                        let run_id = typed_id::new_workflow_run_id();
                        insert_root_run(
                            &mut tx,
                            &app,
                            &run_id,
                            &registration.workflow_name,
                            &row.deploy_id,
                            &StartOptions {
                                input: registration.input.clone(),
                                ..Default::default()
                            },
                            now,
                        )
                        .await?;
                        runs.update(
                            value!({"app_id":app.as_str(), "id":run_id.clone()}),
                            value!({"schedule_id":id.clone()}),
                        )
                        .await?;
                        fired += 1;
                        Some(run_id)
                    };
                    occurrences.insert(value!({"app_id":app.as_str(), "schedule_id":id.clone(), "at":at, "run_id":run_id})).await?;
                }
                at = registration.schedule.next_after(at, anchor)?;
                if at > now {
                    break;
                }
                if index + 1 == cap {
                    at = registration.schedule.next_after(now, anchor)?;
                }
            }
            schedules
                .update(
                    value!({"app_id":app.as_str(), "id":id}),
                    value!({"next_at":at}),
                )
                .await?;
            tx.commit().await?;
        }
        Ok(fired)
    }
}
