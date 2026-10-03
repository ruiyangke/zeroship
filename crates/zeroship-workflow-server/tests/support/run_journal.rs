//! The admission hold that lets a served creator-facing `restart` be read
//! beyond its queued run.
//!
//! [`super::journal::seed_run`] writes the queued run itself; a caller that
//! wants the served path calls this afterward. The deployment-hold suite in
//! `zeroship-control` includes the base journal module and compiles none of
//! this.

#![allow(
    clippy::future_not_send,
    reason = "fixture connections stay on their compio runtime"
)]

use super::platform;
use zeroship_core::{app_id::AppId, typed_id, workflow_deployments::HoldScope};

/// Open admission for the seeded deployment, which is what lets `restart` be
/// SERVED over the wire.
///
/// The hold is what restart was missing, not the deploy: [`super::journal::seed_run`] already
/// writes the active, available `deploys` row that `active_deploy` and
/// `exact_target` read, and `require_journal_hold` then asks
/// `admission_generation` for a `held` intent under `HoldScope::for_app`. With
/// no such row that read refuses and restart answers `Unavailable`, so a caller
/// that wants the refusal seeds the run and does NOT call this.
///
/// The shape is the one `acquire_deployment_hold_checked` leaves behind once its
/// acquisition is acknowledged: the holder that scope names, generation one, and
/// the deployment's own hash. Nothing in this process performs that exchange, so
/// the row stands in for the activation that would have recorded it.
///
/// Returns the deployment the hold opens admission for, read back out of the
/// journal rather than derived a second time, so a caller comparing a pinned
/// deployment compares against the row [`super::journal::seed_run`] wrote.
pub async fn seed_journal_hold(platform: &platform::Platform, app: &AppId) -> String {
    let seeded = platform
        .admin
        .query_one(
            "SELECT id,hash FROM workflow_manager.__zeroship_workflow_deploys \
             WHERE app_id=$1 AND active=1 AND state='available'",
            &[&app.as_str()],
        )
        .await
        .unwrap();
    let deploy: String = seeded.get(0);
    let hash: String = seeded.get(1);
    let scope = HoldScope::for_app(app.clone());
    let inserted = platform
        .admin
        .execute(
            "INSERT INTO workflow_manager.__zeroship_workflow_deployment_holds \
             (id,app_id,deploy_id,deploy_hash,holder_id,generation,state) \
             VALUES($1,$2,$3,$4,$5,1,'held')",
            &[
                &typed_id::generate("wjr"),
                &app.as_str(),
                &deploy,
                &hash,
                &scope.holder(),
            ],
        )
        .await
        .unwrap();
    assert_eq!(inserted, 1, "no deployment hold was recorded");
    deploy
}
