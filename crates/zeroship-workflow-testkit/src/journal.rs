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
use zeroship_core::{app_id::AppId, typed_id, workflow_coordination::RunId};

/// Put a well-formed queued run in the journal.
///
/// Well-formed means what the journal's own readers require, not merely what
/// the columns accept: the generation carries a parseable journal identity and
/// the continuation chain a started run is bound to. A row that satisfies the
/// schema and not the reader refuses as an invalid journal, which reads as a
/// verdict about the call under test rather than about the arrangement.
///
/// This is the ARRANGE step, never the thing under test: every call exercised
/// against it acts on a run that already exists, so the paths under test are
/// the request, the zone, the binding, the epoch and the journal read or
/// write, none of which this seeding touches. The writes a caller asserts go
/// through the endpoint, because seeding around a write would prove nothing
/// about the write.
///
/// The app state and `deploys` rows stand in for the activation that enters an
/// app into this journal and records its deployment, which is the one write on
/// this host that creates either. That activation is itself under test in
/// `maintenance_lane`, for an app this never seeded, so this arrangement cannot
/// hide a journal the service is unable to enter an app into.
pub async fn seed_run(platform: &platform::Platform, app: &AppId) -> RunId {
    let run = RunId::mint();
    // One deploy per app, derived from the app so that repeated calls for one
    // app reuse it and two apps never share it. The row is keyed by id alone,
    // so a literal shared across apps inserts for the first and leaves the
    // next app's run with no deploy of its own to reference.
    let deploy = app.as_str().replacen("app_", "dep_", 1);
    let app = app.as_str().to_owned();
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
    // The id shape `types::storage_id` mints, because the journal parses it:
    // `Generation::validate` requires a `wjr` identity, so a generation named
    // any other way refuses as an invalid continuation journal rather than
    // reading as a generation this run does not have.
    let generation = typed_id::generate("wjr");
    platform.admin.execute(
        "INSERT INTO workflow_manager.__zeroship_workflow_generations(id,app_id,run_id,generation,deploy_id,state,started_at) \
         VALUES($1,$2,$3,0,$4,'queued',0)",
        &[&generation, &app, &run.as_str(), &deploy],
    ).await.unwrap();
    // The head and member `continuations::create` binds a started generation
    // to, at the revision it starts them on. `restart` advances this chain from
    // the run's current generation, so a run that has none is not restartable
    // whatever else the journal holds.
    let head = typed_id::generate("wjr");
    platform.admin.execute(
        "INSERT INTO workflow_manager.__zeroship_workflow_continuation_heads(id,app_id,current_generation_id,revision) \
         VALUES($1,$2,$3,1)",
        &[&head, &app, &generation],
    ).await.unwrap();
    platform.admin.execute(
        "INSERT INTO workflow_manager.__zeroship_workflow_continuation_members(id,app_id,head_id,revision) \
         VALUES($1,$2,$3,1)",
        &[&generation, &app, &head],
    ).await.unwrap();
    run
}
