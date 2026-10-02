//! The journal rows a served creator-facing run call reads beyond its queued
//! run: the admission hold that lets `restart` be served, and the leased
//! dispatch and payload a task-payload read acts on.
//!
//! The deployment-hold suite in `zeroship-control` includes the base journal
//! module and needs only the queued-run seeding, so the served-run arrangements
//! live here where that suite does not compile them.

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

pub struct LeasedTask {
    pub task: String,
    pub token: String,
    pub payload: String,
    pub reference: WorkflowOutputRef,
}
