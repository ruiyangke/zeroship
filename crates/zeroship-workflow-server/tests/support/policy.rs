use super::platform;
use zeroship_core::{AppId, typed_id};

/// Give an app, created on demand, a plan that allows workflows.
pub async fn seed_app(platform: &platform::Platform, app: &AppId) -> String {
    let plan = typed_id::new_plan_id();
    platform.admin.execute("INSERT INTO zeroship.plans(id,name,runtime_limits_json,workflows_allowed) VALUES($1,$2,'{}',true)", &[&plan, &format!("policy-{plan}")]).await.unwrap();
    platform.seed_app(app).await;
    platform.admin.execute("UPDATE zeroship.apps SET plan_id=$2,workflows_enabled=true WHERE id=$1", &[&app.as_str(), &plan]).await.unwrap();
    plan
}
