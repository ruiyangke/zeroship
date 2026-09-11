use super::{
    app::{decode, encode, insert_root_run, lock_app},
    store::Transaction,
    AppPolicy, DeployRegistration, WorkflowService,
};
use crate::{calendar::Calendar, operations::StartOptions, validation, WorkflowServiceError};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use zeroship_core::{app_id::AppId, typed_id};

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
/// the active snapshot preserves its due frontier; replacing a snapshot starts
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
    let schedules = tx.table("schedules");
    let existing = tx
        .query(
            &format!(
                "SELECT id,name,deploy_id,definition,next_at FROM {schedules} WHERE app_id=$1"
            ),
            &[app.as_str().into()],
        )
        .await?;
    for row in &existing {
        if !names.contains(&row.text("name")?) {
            tx.execute(
                &format!("UPDATE {schedules} SET next_at=NULL WHERE app_id=$1 AND id=$2"),
                &[app.as_str().into(), row.text("id")?.into()],
            )
            .await?;
        }
    }
    for registration in &deploy.schedules {
        let encoded = encode(registration)?;
        let mut id = None;
        let mut unchanged = false;
        for row in &existing {
            if row.text("name")? == registration.name {
                id = Some(row.text("id")?);
                unchanged = row.optional_integer("next_at")?.is_some()
                    && row.text("deploy_id")? == deploy.id
                    && decode::<ScheduleRegistration>(&row.text("definition")?)? == *registration;
                break;
            }
        }
        if unchanged {
            continue;
        }
        let next = registration.schedule.next_after(now, now)?;
        if let Some(id) = id {
            tx.execute(&format!("UPDATE {schedules} SET workflow_name=$3,deploy_id=$4,definition=$5,next_at=$6,anchor_at=$7,revision=revision+1 WHERE app_id=$1 AND id=$2"), &[app.as_str().into(),id.into(),registration.workflow_name.clone().into(),deploy.id.clone().into(),encoded.into(),next.into(),now.into()]).await?;
        } else {
            tx.execute(&format!("INSERT INTO {schedules} (app_id,id,name,workflow_name,deploy_id,definition,next_at,anchor_at,revision,last_checked_at) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,0,0)"), &[app.as_str().into(),typed_id::new_workflow_schedule_id().into(),registration.name.clone().into(),registration.workflow_name.clone().into(),deploy.id.clone().into(),encoded.into(),next.into(),now.into()]).await?;
        }
    }
    Ok(())
}

impl WorkflowService {
    /// Invoked by the host maintenance loop; occurrences and runs commit together.
    pub async fn tick_schedules(&self) -> Result<usize, WorkflowServiceError> {
        let mut tx = self.store.begin().await?;
        let now = tx.now().await?;
        let schedules = tx.table("schedules");
        let due=tx.query(&format!("SELECT app_id,id FROM {schedules} WHERE next_at <= $1 ORDER BY last_checked_at,next_at,app_id,id LIMIT 128"), &[now.into()]).await?;
        tx.commit().await?;
        let mut fired = 0;
        for candidate in due {
            let app = AppId::parse(&candidate.text("app_id")?).map_err(|_| {
                WorkflowServiceError::Internal("invalid persisted workflow app identity".into())
            })?;
            let id = candidate.text("id")?;
            let mut tx = self.store.begin().await?;
            let policy = lock_app(&mut tx, &app).await?;
            super::deploys::reconcile_platform(&mut tx, &app, &policy).await?;
            let now = tx.now().await?;
            tx.execute(
                &format!("UPDATE {schedules} SET last_checked_at=$3 WHERE app_id=$1 AND id=$2"),
                &[app.as_str().into(), id.clone().into(), now.into()],
            )
            .await?;
            let rows = tx
                .query(
                    &format!(
                        "SELECT * FROM {schedules} WHERE app_id=$1 AND id=$2{}",
                        tx.lock_clause()
                    ),
                    &[app.as_str().into(), id.clone().into()],
                )
                .await?;
            let Some(row) = rows.first() else {
                tx.commit().await?;
                continue;
            };
            let Some(mut at) = row.optional_integer("next_at")?.filter(|at| *at <= now) else {
                tx.commit().await?;
                continue;
            };
            if !policy.admission {
                tx.commit().await?;
                continue;
            }
            let registration: ScheduleRegistration = decode(&row.text("definition")?)?;
            let anchor = row.integer("anchor_at")?;
            let cap = match registration.catch_up {
                ScheduleCatchUp::Skip => 1,
                ScheduleCatchUp::Backfill { max } => max.min(policy.max_schedule_backfill),
            };
            let runs = tx.table("runs");
            let occurrences = tx.table("occurrences");
            for index in 0..cap {
                let duplicate=tx.query(&format!("SELECT at FROM {occurrences} WHERE app_id=$1 AND schedule_id=$2 AND at=$3"), &[app.as_str().into(),id.clone().into(),at.into()]).await?;
                if duplicate.is_empty() {
                    let overlapping=tx.query(&format!("SELECT id FROM {runs} WHERE app_id=$1 AND schedule_id=$2 AND state NOT IN ('completed','failed','cancelled') LIMIT 1"), &[app.as_str().into(),id.clone().into()]).await?;
                    let skip = registration.overlap == ScheduleOverlap::SkipIfRunning
                        && !overlapping.is_empty();
                    let run_id = if skip {
                        None
                    } else {
                        let live=tx.query(&format!("SELECT COUNT(*) AS total FROM {runs} WHERE app_id=$1 AND state NOT IN ('completed','failed','cancelled')"), &[app.as_str().into()]).await?;
                        if live[0].integer("total")? >= policy.max_live_runs {
                            break;
                        }
                        let run_id = typed_id::new_workflow_run_id();
                        insert_root_run(
                            &mut tx,
                            &app,
                            &run_id,
                            &registration.workflow_name,
                            &row.text("deploy_id")?,
                            &StartOptions {
                                input: registration.input.clone(),
                                ..Default::default()
                            },
                            now,
                        )
                        .await?;
                        tx.execute(
                            &format!("UPDATE {runs} SET schedule_id=$3 WHERE app_id=$1 AND id=$2"),
                            &[
                                app.as_str().into(),
                                run_id.clone().into(),
                                id.clone().into(),
                            ],
                        )
                        .await?;
                        fired += 1;
                        Some(run_id)
                    };
                    tx.execute(&format!("INSERT INTO {occurrences} (app_id,schedule_id,at,run_id) VALUES ($1,$2,$3,$4)"), &[app.as_str().into(),id.clone().into(),at.into(),run_id.into()]).await?;
                }
                at = registration.schedule.next_after(at, anchor)?;
                if at > now {
                    break;
                }
                if index + 1 == cap {
                    at = registration.schedule.next_after(now, anchor)?;
                }
            }
            tx.execute(
                &format!("UPDATE {schedules} SET next_at=$3 WHERE app_id=$1 AND id=$2"),
                &[app.as_str().into(), id.into(), at.into()],
            )
            .await?;
            tx.commit().await?;
        }
        Ok(fired)
    }
}
