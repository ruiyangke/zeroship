use super::{invalid, Transaction, WorkflowServiceError};
use crate::service::models::{deployment_holds, job_publications, reconciliation_scans};
use serde::{Deserialize, Serialize};
use zeroship_data_orm::orm::{FindOptions, FromRow};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum Phase {
    Publications,
    DeploymentHolds,
}

impl Phase {
    pub(super) const fn as_str(self) -> &'static str {
        match self {
            Self::Publications => "publications",
            Self::DeploymentHolds => "deployment_holds",
        }
    }

    pub(super) fn parse(value: &str) -> Result<Self, WorkflowServiceError> {
        match value {
            "publications" => Ok(Self::Publications),
            "deployment_holds" => Ok(Self::DeploymentHolds),
            _ => Err(invalid()),
        }
    }

    pub(super) const fn next(self) -> Self {
        match self {
            Self::Publications => Self::DeploymentHolds,
            Self::DeploymentHolds => Self::Publications,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Plan {
    pub phase: Phase,
    pub revision: i64,
    pub after: Option<String>,
    pub previous_upper: Option<String>,
    pub upper: Option<String>,
    pub ids: Vec<String>,
    pub more: bool,
}

impl Plan {
    pub(super) fn validate(&self) -> Result<(), WorkflowServiceError> {
        if self.revision <= 0
            || i64::try_from(self.ids.len()).map_err(|_| invalid())?
                > zeroship_data_orm::sql::MAX_ROW_LIMIT
            || self
                .previous_upper
                .as_ref()
                .is_some_and(|upper| Some(upper) != self.upper.as_ref())
            || self.after.as_ref().is_some_and(|after| {
                self.previous_upper
                    .as_ref()
                    .is_none_or(|upper| after >= upper)
            })
        {
            return Err(invalid());
        }
        let mut previous = self.after.as_deref();
        for id in &self.ids {
            if previous.is_some_and(|after| after >= id.as_str())
                || self.upper.as_ref().is_none_or(|upper| id > upper)
            {
                return Err(invalid());
            }
            previous = Some(id);
        }
        if self.more
            && self
                .ids
                .last()
                .is_none_or(|last| self.upper.as_ref().is_none_or(|upper| last >= upper))
        {
            return Err(invalid());
        }
        Ok(())
    }

    pub(super) fn check_scan(&self, current: &Scan) -> Result<(), WorkflowServiceError> {
        let phase = Phase::parse(&current.phase)?;
        if current.revision < self.revision
            || (current.revision == self.revision
                && (phase != self.phase
                    || current.after_id != self.after
                    || current.upper_id != self.previous_upper))
        {
            return Err(invalid());
        }
        Ok(())
    }
}

#[derive(FromRow)]
#[orm(entity = reconciliation_scans)]
pub(super) struct Scan {
    pub revision: i64,
    pub phase: String,
    pub after_id: Option<String>,
    pub upper_id: Option<String>,
}

pub(super) async fn scan(
    tx: &Transaction,
    app: &str,
) -> Result<Option<Scan>, WorkflowServiceError> {
    Ok(tx
        .database()
        .entity::<reconciliation_scans::Entity>()?
        .find::<Scan>(
            reconciliation_scans::id.eq(app)?,
            FindOptions {
                limit: Some(1),
                ..Default::default()
            },
        )
        .await?
        .into_iter()
        .next())
}

pub(super) async fn pending_ids(
    tx: &Transaction,
    app: &str,
    phase: Phase,
    after: Option<&str>,
    upper: Option<&str>,
    limit: u32,
    descending: bool,
) -> Result<Vec<String>, WorkflowServiceError> {
    match phase {
        Phase::Publications => publication_ids(tx, app, after, upper, limit, descending).await,
        Phase::DeploymentHolds => hold_ids(tx, app, after, upper, limit, descending).await,
    }
}

#[derive(FromRow)]
#[orm(entity = job_publications)]
struct PublicationId {
    id: String,
}

async fn publication_ids(
    tx: &Transaction,
    app: &str,
    after: Option<&str>,
    upper: Option<&str>,
    limit: u32,
    descending: bool,
) -> Result<Vec<String>, WorkflowServiceError> {
    let source = tx
        .database()
        .entity::<job_publications::Entity>()?
        .alias("p")?;
    let id = source.column(job_publications::id);
    let mut predicate = source.column(job_publications::app_id).eq(app)?.and(
        source
            .column(job_publications::confirmed_at)
            .eq(None::<i64>)?,
    );
    if let Some(after) = after {
        predicate = predicate.and(id.gt(after)?);
    }
    if let Some(upper) = upper {
        predicate = predicate.and(id.lte(upper)?);
    }
    Ok(tx
        .database()
        .from(&source)
        .filter(predicate)
        .order_by(if descending { id.desc() } else { id.asc() })
        .select(source.row::<PublicationId>())?
        .limit(i64::from(limit))?
        .all()
        .await?
        .into_iter()
        .map(|row| row.id)
        .collect())
}

#[derive(FromRow)]
#[orm(entity = deployment_holds)]
struct HoldId {
    deploy_id: String,
}

async fn hold_ids(
    tx: &Transaction,
    app: &str,
    after: Option<&str>,
    upper: Option<&str>,
    limit: u32,
    descending: bool,
) -> Result<Vec<String>, WorkflowServiceError> {
    let source = tx
        .database()
        .entity::<deployment_holds::Entity>()?
        .alias("h")?;
    let id = source.column(deployment_holds::deploy_id);
    let mut predicate = source.column(deployment_holds::app_id).eq(app)?.and(
        source
            .column(deployment_holds::state)
            .in_values(["acquiring", "releasing"])?,
    );
    if let Some(after) = after {
        predicate = predicate.and(id.gt(after)?);
    }
    if let Some(upper) = upper {
        predicate = predicate.and(id.lte(upper)?);
    }
    Ok(tx
        .database()
        .from(&source)
        .filter(predicate)
        .order_by(if descending { id.desc() } else { id.asc() })
        .select(source.row::<HoldId>())?
        .limit(i64::from(limit))?
        .all()
        .await?
        .into_iter()
        .map(|row| row.deploy_id)
        .collect())
}
