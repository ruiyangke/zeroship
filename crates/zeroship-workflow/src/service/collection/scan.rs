use super::{invalid, CollectionOptions, Transaction, WorkflowServiceError};
use crate::service::models::{collection_pages, collection_scans, payloads};
use serde::{Deserialize, Serialize};
use zeroship_core::app_id::AppId;
use zeroship_data_orm::orm::{FindOptions, FromRow, Insertable};

#[derive(FromRow, Insertable)]
#[orm(entity = collection_scans)]
pub(super) struct Scan {
    pub id: String,
    pub revision: i64,
    pub after_id: Option<String>,
    pub upper_id: Option<String>,
    pub observed_at: Option<i64>,
}
impl Scan {
    pub fn validate(&self, app: &AppId) -> Result<(), WorkflowServiceError> {
        if self.id != app.as_str()
            || self.revision <= 0
            || self.observed_at.is_some_and(|value| value < 0)
            || (self.observed_at.is_none() && (self.after_id.is_some() || self.upper_id.is_some()))
            || self
                .after_id
                .as_ref()
                .is_some_and(|after| self.upper_id.as_ref().is_none_or(|upper| after >= upper))
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
        if scan.revision < self.revision
            || (scan.revision == self.revision
                && (scan.after_id != self.after
                    || scan.upper_id != self.upper
                    || scan.observed_at != Some(self.observed_at)))
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

pub(super) async fn read(
    tx: &Transaction,
    app: &AppId,
) -> Result<Option<Scan>, WorkflowServiceError> {
    let row = tx
        .database()
        .entity::<collection_scans::Entity>()?
        .find::<Scan>(
            collection_scans::id.eq(app.as_str())?,
            FindOptions {
                limit: Some(1),
                ..Default::default()
            },
        )
        .await?
        .into_iter()
        .next();
    if let Some(row) = &row {
        row.validate(app)?;
    }
    Ok(row)
}

pub(super) async fn initialize(
    tx: &Transaction,
    app: &AppId,
    now: i64,
) -> Result<Scan, WorkflowServiceError> {
    let mut scan = if let Some(scan) = read(tx, app).await? {
        scan
    } else {
        tx.database()
            .entity::<collection_scans::Entity>()?
            .insert::<_, Scan>(Scan {
                id: app.as_str().to_owned(),
                revision: 1,
                after_id: None,
                upper_id: None,
                observed_at: None,
            })
            .await?
    };
    scan.validate(app)?;
    if scan.observed_at.is_none() {
        scan.upper_id = ids(tx, app, now, None, None, 1, true)
            .await?
            .into_iter()
            .next();
        scan.observed_at = Some(now);
        super::changed_once(
            tx.database()
                .entity::<collection_scans::Entity>()?
                .update_many(
                    collection_scans::id
                        .eq(app.as_str())?
                        .and(collection_scans::revision.eq(scan.revision)?),
                    collection_scans::upper_id
                        .set(scan.upper_id.as_deref())?
                        .and(collection_scans::observed_at.set(Some(now))?)?,
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
    let observed_at = scan.observed_at.ok_or_else(invalid)?;
    let ids = if let Some(upper) = scan.upper_id.as_deref() {
        ids(
            tx,
            app,
            observed_at,
            scan.after_id.as_deref(),
            Some(upper),
            options.page_size,
            false,
        )
        .await?
    } else {
        Vec::new()
    };
    let more = ids.len() == options.page_size as usize
        && ids
            .last()
            .is_some_and(|last| scan.upper_id.as_ref().is_some_and(|upper| last < upper));
    let plan = Plan {
        revision: scan.revision,
        after: scan.after_id.clone(),
        upper: scan.upper_id.clone(),
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
