//! The app-and-policy arrangement a job-delivering fixture publishes before it
//! claims, matching what a deployment provisions.

#![allow(
    clippy::future_not_send,
    reason = "fixture connections stay on their compio runtime"
)]

use super::{
    platform,
    policy::{operator, plan_admin, rollout, seed_app},
};
use zeroship_core::{AppId, workflow_policy::AppPolicy};

/// Seed the app AND publish the operator policy its plan carries.
///
/// A claim reads the app's delivery ceiling from the policy source before it
/// delivers, so an app whose plan carries no policy - or a deployment with no
/// published rollout switches - has no ceiling and every claim is refused
/// `unavailable`. Fixtures that deliver jobs provision both, the way a
/// deployment does, rather than relying on a default the source does not carry.
pub async fn provision(platform: &platform::Platform, app: &AppId, policy: &AppPolicy) -> String {
    let plan = seed_app(platform, app).await;
    let plans = plan_admin(platform).await;
    Box::pin(plans.set_plan_policy(&plan, policy)).await.unwrap();
    let operator = operator(platform).await;
    Box::pin(operator.set_rollout(rollout())).await.unwrap();
    plan
}
