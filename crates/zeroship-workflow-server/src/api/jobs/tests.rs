use super::*;
use std::num::NonZeroU64;
use zeroship_core::workflow_jobs::{
    DeliveryLease, DeploymentId, JobId, JobOperation, JobOutcome, JobReceipt,
};

/// One delivery as a claim reply carries it. Every delivery this builds
/// encodes to the same length, so a room can be sized in deliveries.
fn delivery() -> ClaimedDelivery<AcceptedJob> {
    let job = JobSpec {
        id: JobId::mint(),
        app_id: AppId::mint(),
        operation: JobOperation::Advance {
            deployment_id: DeploymentId::mint(),
            run_id: zeroship_core::workflow_coordination::RunId::mint(),
            generation: 0,
            revision: 1.try_into().unwrap(),
        },
        available_at: 1.try_into().unwrap(),
    };
    ClaimedDelivery {
        lease: DeliveryLease {
            delivery: Delivery {
                job: job.clone(),
                worker_id: WorkerId::mint(),
                attempt: 1.try_into().unwrap(),
                deadline: 1_000_000_000_000.try_into().unwrap(),
            },
            remaining_ms: 30_000.try_into().unwrap(),
            attempt_remaining_ms: 300_000.try_into().unwrap(),
        },
        accepted: Some(AcceptedJob::Settled {
            receipt: Box::new(JobReceipt {
                job,
                outcome: JobOutcome::Completed {},
            }),
        }),
    }
}

fn encoded(value: &impl serde::Serialize) -> usize {
    serde_json::to_vec(value).unwrap().len()
}

/// The room a reply of `deliveries` such deliveries needs: its widest envelope
/// and each delivery with its separator.
fn room_for(deliveries: usize) -> usize {
    let envelope = encoded(&ClaimedJobs::<AcceptedJob> {
        deliveries: Vec::new(),
        after: Some(AppId::mint()),
        lap_complete: false,
    });
    envelope + deliveries * (encoded(&delivery()) + 1)
}

/// A reply takes deliveries while the bound has room for them and refuses the
/// first one that would pass it; the control is a bound with room for one more,
/// which takes that delivery too.
#[test]
fn a_reply_refuses_the_delivery_that_would_pass_its_bound() {
    assert_eq!(encoded(&delivery()), encoded(&delivery()));
    let short = ReplyRoom::new(room_for(2) - 1);
    assert!(short.take(&delivery()));
    assert!(!short.take(&delivery()), "the second delivery passes the bound");

    let exact = ReplyRoom::new(room_for(2));
    assert!(exact.take(&delivery()));
    assert!(exact.take(&delivery()), "the second delivery fits exactly");
    assert!(!exact.take(&delivery()));
}

/// The first delivery always fits, so an app whose journal alone fills a reply
/// is still claimable; the bound applies from the second on.
#[test]
fn a_reply_always_takes_its_first_delivery() {
    let none = ReplyRoom::new(0);
    assert!(none.take(&delivery()));
    assert!(!none.take(&delivery()));
}

/// The bound a worker accepts is the one the service fills to: an empty reply
/// at the protocol bound leaves room for deliveries.
#[test]
fn the_protocol_bound_leaves_room_past_the_envelope() {
    let room = ReplyRoom::new(ClaimJobs::MAX_REPLY_BYTES);
    assert!(room.take(&delivery()));
    assert!(room.take(&delivery()));
}

/// A JSON string whose encoding is exactly `bytes` long.
fn padded(bytes: usize) -> serde_json::Value {
    serde_json::Value::String("x".repeat(bytes - 2))
}

/// One delivery at its largest: a replay journal at the platform's journal
/// ceiling, an inline trigger input at the input ceiling, and every bounded
/// identifier and counter at its widest. The reply holding it alone fits the
/// protocol bound both ends read, so the first delivery the service always
/// sends is one the worker accepts rather than one it refuses and the service
/// redelivers.
///
/// The journal's ceiling governs its steps as the journal stores them, and a
/// step on the wire carries a subset of those fields, so one wire step at the
/// ceiling is the largest journal a delivery can carry.
#[test]
fn a_maximal_first_delivery_fits_the_protocol_bound() {
    use zeroship_core::workflow_policy::{MAX_INPUT_BYTES_CEILING, MAX_JOURNAL_BYTES_CEILING};
    use zeroship_workflow::{
        engine::JournalStep,
        service::{TaskAssignment, TaskToken},
        validation::{STEP_NAME_MAX_BYTES, WORKFLOW_NAME_MAX_BYTES},
        WorkflowInvocation, WorkflowTrigger,
    };

    let run = zeroship_core::workflow_coordination::RunId::mint();
    let workflow_name = "w".repeat(WORKFLOW_NAME_MAX_BYTES);
    let step = |output: usize| JournalStep {
        ordinal: i32::MIN,
        name: "n".repeat(STEP_NAME_MAX_BYTES),
        name_occurrence: i32::MIN,
        kind: "child".into(),
        state: "completed".into(),
        output: Some(padded(output)),
        output_ref: None,
        error: None,
        child_run_id: Some(run.as_str().to_owned()),
        compensation_state: Some("compensated".into()),
    };
    let framing = encoded(&step(2)) - 2;
    let journal = step(MAX_JOURNAL_BYTES_CEILING - framing);
    assert_eq!(encoded(&journal), MAX_JOURNAL_BYTES_CEILING);
    let input = padded(MAX_INPUT_BYTES_CEILING);
    assert_eq!(encoded(&input), MAX_INPUT_BYTES_CEILING);
    let trigger: WorkflowTrigger = serde_json::from_value(serde_json::json!({
        "input": input,
        "startedAt": "2026-10-04T23:59:59.999999999Z",
        "runId": run.as_str(),
        "workflowName": workflow_name,
    }))
    .unwrap();
    let mut maximal = delivery();
    maximal.lease.remaining_ms = NonZeroU64::MAX;
    maximal.lease.attempt_remaining_ms = NonZeroU64::MAX;
    maximal.lease.delivery.deadline = i64::MAX.try_into().unwrap();
    maximal.accepted = Some(AcceptedJob::Execute {
        assignment: Box::new(TaskAssignment {
            id: zeroship_core::typed_id::generate("tsk"),
            token: TaskToken::try_from("f".repeat(64)).unwrap(),
            generation: i64::MIN,
            epoch: i64::MIN,
            deadline: i64::MIN,
            lease_ms: i64::MIN,
            invocation: WorkflowInvocation {
                app_id: AppId::mint().as_str().to_owned(),
                deploy_id: DeploymentId::mint().as_str().to_owned(),
                deploy_hash: "f".repeat(64),
                run_id: run.as_str().to_owned(),
                generation: i64::MIN,
                workflow_name: workflow_name.clone(),
                phase: "compensating".into(),
                trigger,
                journal: vec![journal],
            },
        }),
        remaining_ms: NonZeroU64::MAX,
    });
    let reply = ClaimedJobs {
        deliveries: vec![maximal.clone()],
        after: Some(AppId::mint()),
        lap_complete: false,
    };
    let size = encoded(&reply);
    assert!(
        size > MAX_JOURNAL_BYTES_CEILING + MAX_INPUT_BYTES_CEILING,
        "the delivery is not maximal: {size} bytes"
    );
    assert!(
        size <= ClaimJobs::MAX_REPLY_BYTES,
        "a maximal delivery encodes to {size} bytes, past the {} the worker accepts",
        ClaimJobs::MAX_REPLY_BYTES
    );
    assert!(ReplyRoom::new(ClaimJobs::MAX_REPLY_BYTES).take(&maximal));
}
