//! The journal rows a creator-facing run call acts on.
//!
//! Shared, because two targets act on the SAME run shape from opposite sides:
//! the in-process handler suite builds its requests by hand, and the wire-pair
//! suite drives the native client against the spawned service. A second copy of
//! this seeding would let the two drift, and the drift would look like a
//! disagreement between the halves under test.
#![allow(
    clippy::future_not_send,
    reason = "fixture connections stay on their compio runtime"
)]

use super::platform;
use zeroship_core::{app_id::AppId, workflow_coordination::RunId};

/// Put a well-formed queued run in the journal.
///
/// This is the ARRANGE step, never the thing under test: every call exercised
/// against it acts on a run that already exists, so the paths under test are
/// the request, the placement, the binding, the epoch and the journal read or
/// write, none of which this seeding touches. The writes a caller asserts go
/// through the endpoint, because seeding around a write would prove nothing
/// about the write.
pub async fn seed_run(platform: &platform::Platform, app: &AppId) -> RunId {
    let run = RunId::mint();
    let app = app.as_str().to_owned();
    let deploy = "dep_0seed00000000000000000000";
    // The manifest a `DeployRegistration` decodes from. `restart` reads the
    // active deployment before it reaches the fence, so a stub here would
    // refuse as an invalid record rather than as fenced.
    let hash = "a".repeat(64);
    let manifest =
        serde_json::json!({"id": deploy, "hash": hash, "workflows": ["demo"]}).to_string();
    platform.admin.execute(
        "INSERT INTO workflow_manager.__zeroship_workflow_app_state(id,app_id) VALUES($1,$1) ON CONFLICT DO NOTHING",
        &[&app],
    ).await.unwrap();
    platform.admin.execute(
        "INSERT INTO workflow_manager.__zeroship_workflow_deploys(id,app_id,hash,manifest,created_at,active,state,availability_epoch) \
         VALUES($1,$2,$4,$3,0,1,'available',0) ON CONFLICT DO NOTHING",
        &[&deploy, &app, &manifest, &hash],
    ).await.unwrap();
    platform.admin.execute(
        "INSERT INTO workflow_manager.__zeroship_workflow_runs(id,app_id,workflow_name,deploy_id,generation,state,control,due_at,lease_epoch,cascade,depth,created_at,signal_epoch) \
         VALUES($1,$2,'demo',$3,0,'queued','none',0,0,0,0,0,0)",
        &[&run.as_str(), &app, &deploy],
    ).await.unwrap();
    platform.admin.execute(
        "INSERT INTO workflow_manager.__zeroship_workflow_generations(id,app_id,run_id,generation,deploy_id,state,started_at) \
         VALUES($1,$2,$3,0,$4,'queued',0)",
        &[&format!("gen_{}", run.as_str()), &app, &run.as_str(), &deploy],
    ).await.unwrap();
    run
}
