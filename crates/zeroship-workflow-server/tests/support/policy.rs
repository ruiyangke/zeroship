use super::platform;
use zeroship_core::{AppId, OrganizationId, ProjectId, typed_id};

pub async fn seed_app(platform: &platform::Platform, app: &AppId) -> String {
    let organization = OrganizationId::mint();
    let project = ProjectId::mint();
    let plan = typed_id::new_plan_id();
    platform.admin.execute("INSERT INTO zeroship.plans(id,name,runtime_limits_json,workflows_allowed) VALUES($1,'policy-plan','{}',true)", &[&plan]).await.unwrap();
    platform.admin.execute("INSERT INTO zeroship.organizations(id,slug,name,billing_email) VALUES($1,'policy-test','Policy Test','policy@zeroship.test')", &[&organization.as_str()]).await.unwrap();
    platform.admin.execute("INSERT INTO zeroship.projects(id,organization_id,slug,name) VALUES($1,$2,'default','Policy Test')", &[&project.as_str(), &organization.as_str()]).await.unwrap();
    platform.admin.execute("INSERT INTO zeroship.apps(id,name,plan_id,project_id,organization_id,workflows_enabled) VALUES($1,'policy-app',$2,$3,$4,true)", &[&app.as_str(), &plan, &project.as_str(), &organization.as_str()]).await.unwrap();
    plan
}
