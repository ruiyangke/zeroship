//! A LEASED dispatch of a seeded run, and one object its replay edge names,
//! arranged for the task-payload read.

use super::platform;
use zeroship_core::{
    app_id::AppId,
    typed_id,
    workflow_coordination::{RunId, WorkerId, WorkflowOutputRef},
    workflow_jobs::JobId,
};

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
