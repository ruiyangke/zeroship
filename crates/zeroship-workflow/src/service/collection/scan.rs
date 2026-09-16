use super::{invalid, CollectionOptions, Transaction, WorkflowServiceError};
use crate::service::models::{app_state, collection_pages, payloads};
use serde::{Deserialize, Serialize};
use zeroship_core::app_id::AppId;
use zeroship_data_orm::orm::{FindOptions, FromRow, Insertable};

/// The app's collection sweep, held on its locked `app_state` row.
#[derive(FromRow)]
#[orm(entity = app_state)]
pub(super) struct Scan {
    pub app_id: String,
    pub collection_revision: i64,
    pub collection_after_id: Option<String>,
    pub collection_upper_id: Option<String>,
    pub collection_observed_at: Option<i64>,
}
impl Scan {
    pub fn validate(&self, app: &AppId) -> Result<(), WorkflowServiceError> {
        if self.app_id != app.as_str()
            || self.collection_revision <= 0
            || self.collection_observed_at.is_some_and(|value| value < 0)
            || (self.collection_observed_at.is_none()
                && (self.collection_after_id.is_some() || self.collection_upper_id.is_some()))
            || self.collection_after_id.as_ref().is_some_and(|after| {
                self.collection_upper_id
                    .as_ref()
                    .is_none_or(|upper| after >= upper)
            })
        {
            return Err(invalid());
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Plan {
    pub revision: i64,
    pub after: Option<String>,
    pub upper: Option<String>,
    pub observed_at: i64,
    pub ids: Vec<String>,
    pub more: bool,
}
impl Plan {
    pub fn validate(&self) -> Result<(), WorkflowServiceError> {
        if self.revision <= 0
            || self.observed_at < 0
            || self.ids.len() > super::super::payloads::MAX_COLLECTION_BATCH
            || self
                .after
                .as_ref()
                .is_some_and(|after| self.upper.as_ref().is_none_or(|upper| after >= upper))
        {
            return Err(invalid());
        }
        let mut previous = self.after.as_deref();
        for id in &self.ids {
            if previous.is_some_and(|previous| previous >= id.as_str())
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
    pub fn check_scan(&self, scan: &Scan) -> Result<(), WorkflowServiceError> {
        if scan.collection_revision < self.revision
            || (scan.collection_revision == self.revision
                && (scan.collection_after_id != self.after
                    || scan.collection_upper_id != self.upper
                    || scan.collection_observed_at != Some(self.observed_at)))
        {
            return Err(invalid());
        }
        Ok(())
    }
}

#[derive(FromRow, Insertable)]
#[orm(entity = collection_pages)]
pub(super) struct Page {
    pub id: String,
    pub app_id: String,
    pub plan: String,
    pub next_index: i64,
}

/// Read the sweep from the app's locked state row. Registration creates that
/// row, and every caller locks it first, so its absence is a damaged journal.
pub(super) async fn read(tx: &Transaction, app: &AppId) -> Result<Scan, WorkflowServiceError> {
    let row = tx
        .database()
        .entity::<app_state::Entity>()?
        .find::<Scan>(
            app_state::app_id.eq(app.as_str())?,
            FindOptions {
                limit: Some(1),
                ..Default::default()
            },
        )
        .await?
        .into_iter()
        .next()
        .ok_or_else(invalid)?;
    row.validate(app)?;
    Ok(row)
}

pub(super) async fn initialize(
    tx: &Transaction,
    app: &AppId,
    now: i64,
) -> Result<Scan, WorkflowServiceError> {
    let mut scan = read(tx, app).await?;
    if scan.collection_observed_at.is_none() {
        scan.collection_upper_id = ids(tx, app, now, None, None, 1, true)
            .await?
            .into_iter()
            .next();
        scan.collection_observed_at = Some(now);
        super::changed_once(
            tx.database()
                .entity::<app_state::Entity>()?
                .update_many(
                    app_state::app_id
                        .eq(app.as_str())?
                        .and(app_state::collection_revision.eq(scan.collection_revision)?),
                    app_state::collection_upper_id
                        .set(scan.collection_upper_id.as_deref())?
                        .and(app_state::collection_observed_at.set(Some(now))?)?,
                )
                .await?,
        )?;
    }
    scan.validate(app)?;
    Ok(scan)
}

pub(super) async fn plan(
    tx: &Transaction,
    app: &AppId,
    scan: &Scan,
    options: CollectionOptions,
) -> Result<Plan, WorkflowServiceError> {
    let observed_at = scan.collection_observed_at.ok_or_else(invalid)?;
    let ids = if let Some(upper) = scan.collection_upper_id.as_deref() {
        ids(
            tx,
            app,
            observed_at,
            scan.collection_after_id.as_deref(),
            Some(upper),
            options.page_size,
            false,
        )
        .await?
    } else {
        Vec::new()
    };
    let more = ids.len() == options.page_size as usize
        && ids.last().is_some_and(|last| {
            scan.collection_upper_id
                .as_ref()
                .is_some_and(|upper| last < upper)
        });
    let plan = Plan {
        revision: scan.collection_revision,
        after: scan.collection_after_id.clone(),
        upper: scan.collection_upper_id.clone(),
        observed_at,
        ids,
        more,
    };
    plan.validate()?;
    Ok(plan)
}

#[derive(FromRow)]
#[orm(entity = payloads)]
struct PayloadId {
    id: String,
}

async fn ids(
    tx: &Transaction,
    app: &AppId,
    cutoff: i64,
    after: Option<&str>,
    upper: Option<&str>,
    limit: u32,
    descending: bool,
) -> Result<Vec<String>, WorkflowServiceError> {
    let source = tx.database().entity::<payloads::Entity>()?.alias("p")?;
    let id = source.column(payloads::id);
    let mut filter = source
        .column(payloads::app_id)
        .eq(app.as_str())?
        .and(source.column(payloads::state).in_values([
            "uploading",
            "staged",
            "deleting",
            "deleted",
        ])?)
        .and(source.column(payloads::expires_at).lte(cutoff)?);
    if let Some(after) = after {
        filter = filter.and(id.gt(after)?);
    }
    if let Some(upper) = upper {
        filter = filter.and(id.lte(upper)?);
    }
    Ok(tx
        .database()
        .from(&source)
        .filter(filter)
        .order_by(if descending { id.desc() } else { id.asc() })
        .select(source.row::<PayloadId>())?
        .limit(i64::from(limit))?
        .all()
        .await?
        .into_iter()
        .map(|row| row.id)
        .collect())
}
