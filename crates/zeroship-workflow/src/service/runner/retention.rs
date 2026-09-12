//! Bounded, fair recovery of customer-journal deployment hold intents.

#![expect(
    clippy::future_not_send,
    reason = "recovery uses its host's compio thread"
)]

use crate::{service::WorkflowService, WorkflowServiceError};
use std::{
    collections::{BTreeMap, BTreeSet},
    ops::Bound::{Excluded, Unbounded},
    time::Duration,
};
use zeroship_core::app_id::AppId;

const RECOVERY_BATCH: usize = 64;

/// Cursors are scheduling hints, never authority or evidence of a settled hold.
/// Restart replays pending journal intents through the idempotent protocol.
#[derive(Default)]
pub(super) struct HoldRecovery {
    last_app: Option<AppId>,
    after: BTreeMap<AppId, String>,
}

impl HoldRecovery {
    pub(super) async fn sweep(
        &mut self,
        service: &WorkflowService,
        timeout: Duration,
    ) -> Result<(), WorkflowServiceError> {
        let mut apps: BTreeSet<_> = service.policies.app_ids()?.into_iter().collect();
        self.after.retain(|app, _| apps.contains(app));
        for _ in 0..RECOVERY_BATCH {
            let Some(app) = self
                .last_app
                .as_ref()
                .and_then(|after| apps.range((Excluded(after), Unbounded)).next())
                .or_else(|| apps.first())
                .cloned()
            else {
                break;
            };
            // Advance before journal or platform I/O so timeout and cancellation
            // cannot keep selecting the same app ahead of its peers.
            self.last_app = Some(app.clone());
            match compio::time::timeout(timeout, self.advance(service, &app)).await {
                Ok(Ok(true)) => {}
                Ok(Ok(false)) => {
                    apps.remove(&app);
                }
                Ok(Err(error)) => {
                    tracing::warn!(
                        code = error.code(),
                        "workflow deployment hold recovery failed"
                    );
                    apps.remove(&app);
                }
                Err(_) => {
                    tracing::warn!("workflow deployment hold recovery timed out");
                    apps.remove(&app);
                }
            }
        }
        Ok(())
    }

    async fn advance(
        &mut self,
        service: &WorkflowService,
        app: &AppId,
    ) -> Result<bool, WorkflowServiceError> {
        let client = service
            .deployments
            .as_ref()
            .ok_or(WorkflowServiceError::PermissionDenied)?
            .client(app)?;
        let pending = service
            .pending_deployment_holds(app, self.after.get(app).map(String::as_str), 1)
            .await?;
        let Some(deployment) = pending.into_iter().next() else {
            self.after.remove(app);
            return Ok(false);
        };
        // A malformed intent or failed reply must not hide later intents. Only
        // the service's complete receipt check can settle the durable state.
        self.after.insert(app.clone(), deployment.clone());
        service
            .reconcile_deployment_hold(app, &deployment, client.as_ref())
            .await?;
        Ok(true)
    }
}
