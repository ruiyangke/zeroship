//! The queue scope a lifecycle publication creates, seeded for contracts.
//!
//! Included by this crate's end-to-end target and by `zeroship-control`'s
//! deployment-hold suite, which seed the scope a claim or run call is
//! authorized against. The in-process workflow-server target places apps
//! through the manager and compiles no scope seeding.

use super::platform::Platform;
use zeroship_core::app_id::AppId;

impl Platform {
    /// Register `app`'s queue scope in `zone`, the row Control's schedule
    /// registration writes. Idempotent, so a repeated seed is a no-op.
    pub async fn seed_scope(&self, app: &AppId, zone: &str) {
        let inserted = self
            .admin
            .execute(
                "INSERT INTO workflow_manager.queue_scopes(id,execution_zone_id) \
                 VALUES($1,$2) ON CONFLICT (id) DO NOTHING",
                &[&app.as_str(), &zone],
            )
            .await
            .unwrap();
        assert!(inserted <= 1);
    }
}
