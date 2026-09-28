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
    app_id::AppId,
    typed_id,
    workflow_coordination::{RunId, WorkerId, WorkflowOutputRef},
    workflow_jobs::JobId,
    workflow_deployments::HoldScope,
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

/// A LEASED dispatch of a seeded run, and one object its replay edge names.
///
/// Arranged for the task payload read, whose authority is the dispatch rather
/// than a placement, so the row shape is what `authorized_task` and
/// `owned_reference` require rather than what the columns accept:
///
/// - the task is `leased` with a deadline ahead of the journal's clock, and the
///   run points BACK at it, because `validate_live` compares the run's
///   `task_id`, `generation`, `lease_epoch` and `frontier_revision` against the
///   task's. A task the run does not point at reads as a stale lease, which
///   would look like a verdict about the call rather than about the seeding.
/// - the token is stored only as a `sha256`, through the journal's own `hash`,
///   so the fixture cannot disagree with the comparison it is arranging for.
/// - the payload is `referenced` with an edge on this run, which is the arm of
///   `owned_reference` a committed replay read takes.
/// - `worker` is the caller's own, because a task row is keyed by it and the
///   served read substitutes the identity that SIGNED the request in its place.
///   Seeding any other spelling finds no row at all, which is indistinguishable
///   from the refusal a caller might be trying to assert.
///
/// Returns the dispatch, its token, and the descriptor a caller names -- plus
/// the payload id the journal should answer with, so the caller compares against
/// the row written here rather than against a second derivation of it.
#[allow(
    dead_code,
    reason = "only the suite that reads a task payload needs a leased dispatch"
)]
pub async fn seed_leased_task(
    platform: &platform::Platform,
    app: &AppId,
    run: &RunId,
    worker: &WorkerId,
) -> LeasedTask {
    // The prefixes the journal MINTS these with, named rather than spelled:
    // `inspect_task` parses the dispatch id against its prefix before any lookup,
    // so a fixture id shaped any other way is refused as a task that does not
    // exist -- indistinguishable from the arrangement not being there at all.
    let task = typed_id::generate(typed_id::WORKFLOW_DISPATCH_PREFIX);
    // The delivery coordinates a dispatch claimed under a job carries.
    // `authorize_task` compares all three against the delivery a caller presents,
    // so a task seeded without them can be read from but never released.
    //
    // The job SPECIFICATION is written too, and not as decoration: `tasks.job_id`
    // is a foreign key into `job_receipts`, so a delivered task cannot exist
    // without its logical job's row, and `Record::receipt` compares that stored
    // specification against the job a caller names byte for byte. `outcome` stays
    // NULL, which is what "claimed but not yet committed" looks like.
    let job = JobId::mint();
    let attempt: i64 = 1;
    let assignment_revision: i64 = 1;
    let specification = serde_json::json!({
        "id": job.as_str(),
        "appId": app.as_str(),
        "operation": {
            "kind": "advance",
            "deploymentId": app.as_str().replacen("app_", "dep_", 1),
            "runId": run.as_str(),
            "generation": 0,
            "revision": 1,
        },
        "availableAt": 0,
    })
    .to_string();
    let payload = typed_id::generate(typed_id::WORKFLOW_PAYLOAD_PREFIX);
    // The shape `TaskToken`'s own parse requires: 64 lowercase hex characters.
    // Anything else is refused as unauthenticated before a lookup happens, so a
    // fixture token that does not parse would test the parse and nothing else.
    let token = "5".repeat(64);
    let token_hash = zeroship_workflow::service::hash(token.as_bytes());
    let hash = "b".repeat(64);
    let size: i64 = 11;
    let app = app.as_str().to_owned();
    // Far enough ahead that no plausible fixture latency expires the lease, and
    // expressed in the journal's own unit: `Transaction::now` reads epoch
    // milliseconds from the database clock.
    let deadline: i64 = 4_000_000_000_000;
    platform.admin.execute(
        "INSERT INTO workflow_manager.__zeroship_workflow_job_receipts(id,app_id,run_id,specification,created_at) \
         VALUES($1,$2,$3,$4,0)",
        &[&job.as_str(), &app, &run.as_str(), &specification],
    ).await.unwrap();
    platform.admin.execute(
        "INSERT INTO workflow_manager.__zeroship_workflow_tasks(id,app_id,run_id,generation,worker,epoch,token_hash,deadline,state,frontier_revision,created_at,job_id,delivery_attempt,assignment_revision) \
         VALUES($1,$2,$3,0,$4,0,$5,$6,'leased',1,0,$7,$8,$9)",
        &[&task, &app, &run.as_str(), &worker.as_str(), &token_hash, &deadline,
          &job.as_str(), &attempt, &assignment_revision],
    ).await.unwrap();
    platform
        .admin
        .execute(
            "UPDATE workflow_manager.__zeroship_workflow_runs SET task_id=$1 \
             WHERE app_id=$2 AND id=$3",
            &[&task, &app, &run.as_str()],
        )
        .await
        .unwrap();
    platform.admin.execute(
        "INSERT INTO workflow_manager.__zeroship_workflow_payloads(id,app_id,run_id,generation,task_id,request_id,hash,size,content_type,state,created_at,expires_at) \
         VALUES($1,$2,$3,0,$4,$5,$6,$7,'application/json','referenced',0,$8)",
        &[&payload, &app, &run.as_str(), &task, &typed_id::generate(typed_id::WORKFLOW_REQUEST_PREFIX), &hash, &size, &deadline],
    ).await.unwrap();
    platform.admin.execute(
        "INSERT INTO workflow_manager.__zeroship_workflow_payload_refs(id,app_id,run_id,generation,slot,ordinal,payload_id) \
         VALUES($1,$2,$3,0,'step',0,$4)",
        &[&typed_id::generate("wjr"), &app, &run.as_str(), &payload],
    ).await.unwrap();
    LeasedTask {
        task,
        token,
        payload,
        reference: WorkflowOutputRef {
            hash,
            size,
            content_type: Some("application/json".to_owned()),
        },
    }
}

#[allow(
    dead_code,
    reason = "only the suite that reads a task payload needs every field"
)]
pub struct LeasedTask {
    pub task: String,
    pub token: String,
    pub payload: String,
    pub reference: WorkflowOutputRef,
}
