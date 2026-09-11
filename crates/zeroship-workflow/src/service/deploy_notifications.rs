//! Durable delivery cursor for Control's deployment outbox.

use super::WorkflowService;
use crate::WorkflowServiceError;
use zeroship_core::{app_id::AppId, typed_id};

/// The cursor is only a fairness hint. Acknowledgements live with the journal,
/// so losing the cursor on restart only rechecks pending notifications.
#[derive(Debug, Clone, Default)]
pub struct DeployReconciliation {
    pub next: Option<DeployReconcileCursor>,
    pub reconciled: usize,
}

#[derive(Debug, Clone)]
pub struct DeployReconcileCursor {
    after: uuid::Uuid,
    through: uuid::Uuid,
}

impl WorkflowService {
    /// Reconcile a bounded page of pending deployment notifications. A failed
    /// app does not prevent later apps from advancing; the next pass retries it.
    pub async fn reconcile_pending_deploys(
        &self,
        cursor: Option<DeployReconcileCursor>,
        limit: usize,
    ) -> Result<DeployReconciliation, WorkflowServiceError> {
        let limit = i64::try_from(limit)
            .ok()
            .filter(|limit| *limit > 0)
            .ok_or_else(|| {
                WorkflowServiceError::InvalidRequest(
                    "deployment reconciliation batch must be positive".into(),
                )
            })?;
        let mut tx = self.store.begin().await?;
        if tx.platform_policy.is_none() {
            return Err(WorkflowServiceError::InvalidRequest(
                "deployment notifications require platform authority".into(),
            ));
        }
        let (after, through) = if let Some(cursor) = cursor {
            (Some(cursor.after), cursor.through)
        } else {
            let latest = tx.query("SELECT app_id::text AS app_id FROM zeroship.workflow_deploy_notifications ORDER BY app_id DESC LIMIT 1", &[]).await?;
            let Some(latest) = latest.first() else {
                tx.commit().await?;
                return Ok(DeployReconciliation::default());
            };
            (
                None,
                uuid::Uuid::parse_str(&latest.text("app_id")?).map_err(|_| invalid_identity())?,
            )
        };
        // Freeze the pass boundary so continuous app creation cannot keep an
        // earlier failed notification waiting behind an ever-growing cursor.
        let rows = tx
            .query(
                "SELECT n.app_id::text AS app_id FROM zeroship.workflow_deploy_notifications n \
             LEFT JOIN workflow.app_state s ON s.platform_app_id=n.app_id::text \
             WHERE n.revision>COALESCE(s.deploy_revision,0) \
             AND ($1::text IS NULL OR n.app_id>$1::text::uuid) AND n.app_id<=$2::text::uuid \
             ORDER BY n.app_id LIMIT $3",
                &[
                    after.map(|id| id.to_string()).into(),
                    through.to_string().into(),
                    limit.into(),
                ],
            )
            .await?;
        tx.commit().await?;
        let mut result = DeployReconciliation::default();
        let end_of_pass = rows.len() < limit as usize;
        for row in rows {
            let source = row.text("app_id")?;
            let uuid = uuid::Uuid::parse_str(&source).map_err(|_| invalid_identity())?;
            let app = AppId::parse(&format!(
                "{}_{}",
                AppId::PREFIX,
                typed_id::uuid_to_base62(&uuid)
            ))
            .map_err(|_| invalid_identity())?;
            result.next = Some(DeployReconcileCursor {
                after: uuid,
                through,
            });
            match self.reconcile_deploy(&app).await {
                Ok(()) => result.reconciled += 1,
                Err(error) => {
                    tracing::warn!(app_id = %app.as_str(), %error, "workflow deployment reconciliation deferred")
                }
            }
        }
        if end_of_pass {
            result.next = None;
        }
        Ok(result)
    }
}

pub(super) async fn revision(
    tx: &mut super::store::Transaction,
    app: &AppId,
) -> Result<Option<i64>, WorkflowServiceError> {
    let rows = tx.query(
        "SELECT revision FROM zeroship.workflow_deploy_notifications WHERE app_id=$1::text::uuid",
        &[app.uuid().to_string().into()],
    ).await?;
    rows.first().map(|row| row.integer("revision")).transpose()
}

pub(super) async fn acknowledge(
    tx: &mut super::store::Transaction,
    app: &AppId,
    revision: Option<i64>,
) -> Result<(), WorkflowServiceError> {
    if let Some(revision) = revision {
        if revision <= 0 {
            return Err(invalid_identity());
        }
        tx.execute(
            "UPDATE workflow.app_state SET deploy_revision=$2 WHERE app_id=$1",
            &[app.as_str().into(), revision.into()],
        )
        .await?;
    }
    Ok(())
}

fn invalid_identity() -> WorkflowServiceError {
    WorkflowServiceError::Unavailable("invalid workflow deployment notification".into())
}
