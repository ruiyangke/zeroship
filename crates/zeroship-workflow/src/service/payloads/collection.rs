//! Eligibility fencing shared by delivered collection and local maintenance.

#![expect(
    clippy::future_not_send,
    reason = "payload cleanup uses compio-local transactions and storage"
)]

use super::{
    AppId, Transaction, WorkflowService, WorkflowServiceError, deadline, lock_app, models, payload,
    storage, storage_error, typed_id,
};

impl WorkflowService {
    pub(in crate::service) async fn collect_payload_checked(
        &self,
        app: &AppId,
        id: &str,
        cutoff: i64,
        check: &impl Fn() -> Result<(), WorkflowServiceError>,
    ) -> Result<bool, WorkflowServiceError> {
        check()?;
        typed_id::parse_with_prefix(id, typed_id::WORKFLOW_PAYLOAD_PREFIX)
            .map_err(|_| invalid())?;
        let storage = storage(self)?;
        let Some((expires_at, retention)) =
            Box::pin(self.fence_payload(app, id, cutoff, check)).await?
        else {
            return Ok(false);
        };
        check()?;
        storage
            .delete(app.as_str(), id)
            .await
            .map_err(storage_error)?;
        check()?;
        let mut tx = self.begin().await?;
        lock_app(&mut tx, app).await?;
        check()?;
        let row = payload(&tx, app, id).await?;
        require_unreferenced(&tx, app, id).await?;
        if row.expires_at > expires_at && matches!(row.state.as_str(), "deleting" | "deleted") {
            check()?;
            tx.commit().await?;
            return Ok(false);
        }
        if row.state != "deleting" || row.expires_at != expires_at {
            return Err(invalid());
        }
        let now = tx.now().await?;
        let next = deadline(now.max(expires_at), retention)?;
        let changed = tx
            .database()
            .entity::<models::payloads::Entity>()?
            .update_many(
                models::payloads::app_id
                    .eq(app.as_str())?
                    .and(models::payloads::id.eq(id)?)
                    .and(models::payloads::state.eq("deleting")?)
                    .and(models::payloads::expires_at.eq(expires_at)?),
                models::payloads::state
                    .set("deleted")?
                    .and(models::payloads::expires_at.set(next)?)?,
            )
            .await?;
        changed_once(changed)?;
        check()?;
        tx.commit().await?;
        check()?;
        Ok(true)
    }

    async fn fence_payload(
        &self,
        app: &AppId,
        id: &str,
        cutoff: i64,
        check: &impl Fn() -> Result<(), WorkflowServiceError>,
    ) -> Result<Option<(i64, i64)>, WorkflowServiceError> {
        let mut tx = self.begin().await?;
        let policy = lock_app(&mut tx, app).await?;
        check()?;
        let now = tx.now().await?;
        let row = match payload(&tx, app, id).await {
            Ok(row) => row,
            Err(WorkflowServiceError::NotFound(_)) => return Ok(None),
            Err(error) => return Err(error),
        };
        if !matches!(
            row.state.as_str(),
            "uploading" | "staged" | "deleting" | "deleted"
        ) || row.expires_at > now.min(cutoff)
        {
            check()?;
            tx.commit().await?;
            return Ok(None);
        }
        require_unreferenced(&tx, app, id).await?;
        let changed = tx
            .database()
            .entity::<models::payloads::Entity>()?
            .update_many(
                models::payloads::app_id
                    .eq(app.as_str())?
                    .and(models::payloads::id.eq(id)?)
                    .and(models::payloads::state.eq(row.state.as_str())?)
                    .and(models::payloads::expires_at.eq(row.expires_at)?),
                models::payloads::state.set("deleting")?,
            )
            .await?;
        changed_once(changed)?;
        check()?;
        tx.commit().await?;
        check()?;
        Ok(Some((row.expires_at, policy.payload_staging_retention_ms)))
    }
}

async fn require_unreferenced(
    tx: &Transaction,
    app: &AppId,
    id: &str,
) -> Result<(), WorkflowServiceError> {
    if tx
        .database()
        .entity::<models::payload_refs::Entity>()?
        .exists(
            models::payload_refs::app_id
                .eq(app.as_str())?
                .and(models::payload_refs::payload_id.eq(id)?),
        )
        .await?
    {
        return Err(invalid());
    }
    Ok(())
}

fn changed_once(changed: i64) -> Result<(), WorkflowServiceError> {
    if changed == 1 { Ok(()) } else { Err(invalid()) }
}

fn invalid() -> WorkflowServiceError {
    WorkflowServiceError::Internal("invalid workflow payload collection fence".into())
}
