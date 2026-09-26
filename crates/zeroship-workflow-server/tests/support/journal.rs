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
use zeroship_core::{
    app_id::AppId, typed_id, workflow_coordination::RunId, workflow_deployments::HoldScope,
};

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
/// the request, the placement, the binding, the epoch and the journal read or
/// write, none of which this seeding touches. The writes a caller asserts go
/// through the endpoint, because seeding around a write would prove nothing
/// about the write.
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

/// Open admission for the seeded deployment, which is what lets `restart` be
/// SERVED over the wire.
///
/// The hold is what restart was missing, not the deploy: [`seed_run`] already
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
/// deployment compares against the row [`seed_run`] wrote.
#[allow(
    dead_code,
    reason = "only the suite that serves restart needs admission opened"
)]
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
