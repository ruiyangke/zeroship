use super::platform;
use zeroship_core::AppId;

/// An app, created on demand, whose plan and app flag both allow workflows.
/// Returns the plan that carries the app's workflow policy.
pub async fn seed_app(platform: &platform::Platform, app: &AppId) -> String {
    let plan = platform.seed_app(app).await;
    platform.admin.execute("UPDATE zeroship.apps SET workflows_enabled=true WHERE id=$1", &[&app.as_str()]).await.unwrap();
    plan
}
