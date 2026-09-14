//! Read the published registry in a consistent snapshot.

use zeroship_data_orm::orm::{Database, DbError, IsolationLevel, TransactionOptions};
use zeroship_data_orm::sql::MAX_ROW_LIMIT;

use crate::store::native::models::signing_keys as model;

#[derive(zeroship_data_orm::orm::FromRow)]
#[orm(entity = crate::store::native::models::signing_keys)]
pub(super) struct PublishedKey {
    id: i64,
    pub kid: String,
    pub public_jwk: zeroship_data_orm::Value,
}

#[allow(
    clippy::future_not_send,
    reason = "the ORM belongs to this compio runtime"
)]
pub(super) async fn published_keys(db: &Database) -> Result<Vec<PublishedKey>, DbError> {
    db.transaction_with_options(
        TransactionOptions::default().isolation_level(IsolationLevel::RepeatableRead),
        |tx| async move {
            let registry = tx.entity::<model::Entity>()?;
            let key = registry.alias("published")?;
            let anchor = registry.alias("anchor")?;
            let mut rows = Vec::new();
            let mut cursor = None;
            loop {
                let mut query = tx
                    .from(&key)
                    .filter(
                        key.column(model::status)
                            .in_values(["active", "next", "retiring"])?,
                    )
                    .order_by(key.column(model::status).asc())
                    .order_by(key.column(model::created_at).desc())
                    .order_by(key.column(model::kid).asc())
                    .limit(MAX_ROW_LIMIT)?;
                if let Some(id) = cursor {
                    // Compare against the stored row so timestamp precision stays in SQL.
                    let after = key
                        .column(model::status)
                        .gt(anchor.column(model::status))?
                        .or(key
                            .column(model::status)
                            .eq(anchor.column(model::status))?
                            .and(
                                key.column(model::created_at)
                                    .lt(anchor.column(model::created_at))?
                                    .or(key
                                        .column(model::created_at)
                                        .eq(anchor.column(model::created_at))?
                                        .and(
                                            key.column(model::kid).gt(anchor.column(model::kid))?,
                                        )),
                            ));
                    query = query
                        .inner_join(&anchor, anchor.column(model::id).eq(id)?)?
                        .filter(after);
                }
                let page = query.select(key.row::<PublishedKey>())?.all().await?;
                cursor = page.last().map(|row| row.id);
                let complete = page.len() < usize::try_from(MAX_ROW_LIMIT).unwrap();
                rows.extend(page);
                if complete {
                    return Ok(rows);
                }
            }
        },
    )
    .await
}
