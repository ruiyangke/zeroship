use serde::{de::DeserializeOwned, Serialize};
use serde_json::{json, Value};
use std::fmt::Debug;
use zeroship_core::{
    app_id::AppId,
    workflow_coordination::{AssignedScope, RequestId, RunId, WorkerId},
    workflow_jobs::{
        Delivery, DeliveryLease, DeploymentId, JobId, JobOperation, JobOutcome, JobSpec,
        Settlement, SettlementReceipt, SubmitJob,
    },
};

fn round_trip<T: Debug + PartialEq + Serialize + DeserializeOwned>(value: &T) -> Value {
    let wire = serde_json::to_value(value).unwrap();
    assert_eq!(&serde_json::from_value::<T>(wire.clone()).unwrap(), value);
    wire
}

fn refuses<T: DeserializeOwned>(wire: Value) {
    assert!(
        serde_json::from_value::<T>(wire.clone()).is_err(),
        "unexpectedly accepted metadata: {wire}"
    );
}

fn operations() -> Vec<(JobOperation, Value)> {
    let run = RunId::mint();
    let request = RequestId::mint();
    vec![
        (
            JobOperation::Advance {
                run_id: run.clone(),
                generation: 0,
                revision: 1.try_into().unwrap(),
            },
            json!({"kind":"advance","runId":run,"generation":0,"revision":1}),
        ),
        (
            JobOperation::Cron {
                request_id: request.clone(),
                run_id: run.clone(),
                revision: 2.try_into().unwrap(),
                scheduled_at: 123.try_into().unwrap(),
            },
            json!({"kind":"cron","requestId":request,"runId":run,"revision":2,"scheduledAt":123}),
        ),
        (
            JobOperation::Management {
                request_id: request.clone(),
                run_id: run.clone(),
            },
            json!({"kind":"management","requestId":request,"runId":run}),
        ),
        (JobOperation::Reconcile {}, json!({"kind":"reconcile"})),
        (JobOperation::Collect {}, json!({"kind":"collect"})),
    ]
}

fn settlement(operation: JobOperation) -> Settlement {
    let job = JobSpec {
        id: JobId::mint(),
        app_id: AppId::mint(),
        deployment_id: DeploymentId::mint(),
        operation,
        available_at: 0.try_into().unwrap(),
    };
    let successor = JobSpec {
        id: JobId::mint(),
        available_at: 321.try_into().unwrap(),
        ..job.clone()
    };
    Settlement {
        delivery: Delivery {
            job,
            worker_id: WorkerId::mint(),
            assignment_revision: 2.try_into().unwrap(),
            attempt: 3.try_into().unwrap(),
            deadline: 456.try_into().unwrap(),
        },
        outcome: JobOutcome::Waiting,
        successors: vec![successor],
    }
}

#[test]
fn operation_wire_shapes_are_explicit_and_round_trip() {
    let cases = operations();
    assert!(!cases.is_empty());
    for (operation, expected) in cases {
        assert_eq!(round_trip(&operation), expected);
    }
    for (outcome, expected) in [
        (JobOutcome::Completed, "completed"),
        (JobOutcome::Waiting, "waiting"),
        (JobOutcome::Rejected, "rejected"),
    ] {
        assert_eq!(round_trip(&outcome), json!(expected));
    }
}

#[test]
fn delivery_and_settlement_preserve_logical_and_attempt_identities() {
    let settlement = settlement(JobOperation::Collect {});
    let delivery = &settlement.delivery;
    let job = &delivery.job;
    let expected_job = json!({
        "id":job.id,"appId":job.app_id,"deploymentId":job.deployment_id,
        "operation":{"kind":"collect"},"availableAt":0,
    });
    assert_eq!(round_trip(job), expected_job);
    let expected_delivery = json!({
        "job":expected_job,"workerId":delivery.worker_id,
        "assignmentRevision":2,"attempt":3,"deadline":456,
    });
    assert_eq!(round_trip(delivery), expected_delivery);
    let successor = &settlement.successors[0];
    assert_ne!(successor.id, job.id);
    assert_eq!(
        round_trip(&settlement),
        json!({
            "delivery":expected_delivery,"outcome":"waiting",
            "successors":[{
                "id":successor.id,"appId":job.app_id,"deploymentId":job.deployment_id,
                "operation":{"kind":"collect"},"availableAt":321,
            }],
        })
    );
    let receipt = SettlementReceipt {
        job_id: job.id.clone(),
        app_id: job.app_id.clone(),
        attempt: delivery.attempt,
        outcome: settlement.outcome,
    };
    assert_eq!(
        round_trip(&receipt),
        json!({"jobId":job.id,"appId":job.app_id,"attempt":3,"outcome":"waiting"})
    );
    round_trip(&Settlement {
        successors: Vec::new(),
        outcome: JobOutcome::Completed,
        ..settlement
    });
}

#[test]
fn worker_publication_and_lease_replies_are_closed_and_carry_no_caller_expiry() {
    let value = settlement(JobOperation::Reconcile {});
    let request = SubmitJob {
        scope: AssignedScope {
            app_id: value.delivery.job.app_id.clone(),
            assignment_revision: value.delivery.assignment_revision,
        },
        job: value.delivery.job.clone(),
    };
    let wire = round_trip(&request);
    assert_eq!(
        wire,
        json!({"scope":{
        "appId":request.scope.app_id,"assignmentRevision":request.scope.assignment_revision,
    },"job":request.job})
    );
    for path in ["", "/scope", "/job", "/job/operation"] {
        for field in ["expiresAt", "input", "databaseUrl", "credentials"] {
            let mut invalid = wire.clone();
            invalid
                .pointer_mut(path)
                .unwrap()
                .as_object_mut()
                .unwrap()
                .insert(field.into(), json!("untrusted"));
            refuses::<SubmitJob>(invalid);
        }
    }
    let lease = DeliveryLease {
        delivery: value.delivery,
        remaining_ms: std::num::NonZeroU64::new(789).unwrap(),
    };
    let wire = round_trip(&lease);
    assert_eq!(wire, json!({"delivery":lease.delivery,"remainingMs":789}));
    for bad in [json!(0), json!(-1), json!(0.5), json!("1"), Value::Null] {
        let mut invalid = wire.clone();
        invalid["remainingMs"] = bad;
        refuses::<DeliveryLease>(invalid);
    }
    for path in ["", "/delivery", "/delivery/job", "/delivery/job/operation"] {
        let mut invalid = wire.clone();
        invalid
            .pointer_mut(path)
            .unwrap()
            .as_object_mut()
            .unwrap()
            .insert("input".into(), json!({"secret":true}));
        refuses::<DeliveryLease>(invalid);
    }
    for field in ["delivery", "remainingMs"] {
        let mut missing = wire.clone();
        missing.as_object_mut().unwrap().remove(field);
        refuses::<DeliveryLease>(missing);
    }
}

#[test]
fn customer_data_is_rejected_at_every_message_and_operation_boundary() {
    for (operation, _) in operations() {
        let value = settlement(operation);
        let wire = round_trip(&value);
        for path in [
            "",
            "/delivery",
            "/delivery/job",
            "/delivery/job/operation",
            "/successors/0",
            "/successors/0/operation",
        ] {
            for field in [
                "input",
                "history",
                "body",
                "result",
                "error",
                "databaseUrl",
                "payloadUrl",
                "credentials",
            ] {
                let mut injected = wire.clone();
                injected
                    .pointer_mut(path)
                    .unwrap()
                    .as_object_mut()
                    .unwrap()
                    .insert(field.into(), json!({"private":"customer-data"}));
                refuses::<Settlement>(injected);
            }
        }
    }
    let receipt = SettlementReceipt {
        job_id: JobId::mint(),
        app_id: AppId::mint(),
        attempt: 1.try_into().unwrap(),
        outcome: JobOutcome::Rejected,
    };
    let wire = round_trip(&receipt);
    for field in ["input", "history", "body", "result", "error"] {
        let mut injected = wire.clone();
        injected[field] = json!({"private":"customer-data"});
        refuses::<SettlementReceipt>(injected);
    }
}

#[test]
fn job_and_deployment_ids_refuse_other_entities_and_malformed_wire_values() {
    let job = JobId::mint();
    let deployment = DeploymentId::mint();
    assert_eq!(JobId::PREFIX, "wjb");
    assert_eq!(DeploymentId::PREFIX, "dep");
    assert_eq!(JobId::parse(job.as_str()).unwrap(), job);
    assert_eq!(
        DeploymentId::parse(deployment.as_str()).unwrap(),
        deployment
    );
    assert_eq!(round_trip(&job), json!(job.as_str()));
    assert_eq!(round_trip(&deployment), json!(deployment.as_str()));
    assert!(JobId::parse(deployment.as_str()).is_err());
    assert!(DeploymentId::parse(job.as_str()).is_err());
    refuses::<JobId>(json!(deployment));
    refuses::<DeploymentId>(json!(job));
    for malformed in [
        "",
        "0191e7a2-b3c4-4d5e-8f90-123456789abc",
        "run_0000000000000000000000",
        "wjb_",
        "dep_",
        "wjb_000000000000000000000",
        "dep_00000000000000000000000",
        "wjb_ZZZZZZZZZZZZZZZZZZZZZZ",
        "dep_ZZZZZZZZZZZZZZZZZZZZZZ",
        "wjb_000000000000000000000!",
        "dep_000000000000000000000!",
    ] {
        assert!(JobId::parse(malformed).is_err());
        assert!(DeploymentId::parse(malformed).is_err());
        refuses::<JobId>(json!(malformed));
        refuses::<DeploymentId>(json!(malformed));
    }
    for malformed in [Value::Null, json!(1), json!({"id":"opaque"})] {
        refuses::<JobId>(malformed.clone());
        refuses::<DeploymentId>(malformed);
    }
}

#[test]
fn nested_identifiers_cannot_be_replaced_with_other_entity_types() {
    for (operation, _) in operations() {
        let wire = round_trip(&settlement(operation));
        for (path, foreign) in [
            ("/delivery/job/id", json!(DeploymentId::mint())),
            ("/delivery/job/deploymentId", json!(JobId::mint())),
            ("/delivery/job/appId", json!(WorkerId::mint())),
            ("/delivery/workerId", json!(AppId::mint())),
            ("/delivery/job/operation/runId", json!(RequestId::mint())),
            ("/delivery/job/operation/requestId", json!(RunId::mint())),
            ("/successors/0/id", json!(DeploymentId::mint())),
        ] {
            if wire.pointer(path).is_none() {
                continue;
            }
            let mut invalid = wire.clone();
            *invalid.pointer_mut(path).unwrap() = foreign;
            refuses::<Settlement>(invalid);
        }
    }
}

#[test]
fn counters_and_deadlines_enforce_native_ranges_on_the_wire() {
    let operation = JobOperation::Advance {
        run_id: RunId::mint(),
        generation: 0,
        revision: 1.try_into().unwrap(),
    };
    let wire = round_trip(&settlement(operation));
    for path in [
        "/delivery/assignmentRevision",
        "/delivery/attempt",
        "/delivery/job/operation/revision",
        "/successors/0/operation/revision",
    ] {
        for bad in [
            json!(0),
            json!(-1),
            json!(u64::MAX),
            json!(1.5),
            json!("1"),
            Value::Null,
        ] {
            let mut invalid = wire.clone();
            *invalid.pointer_mut(path).unwrap() = bad;
            refuses::<Settlement>(invalid);
        }
        let mut maximum = wire.clone();
        *maximum.pointer_mut(path).unwrap() = json!(i64::MAX);
        round_trip(&serde_json::from_value::<Settlement>(maximum).unwrap());
    }
    for bad in [
        json!(-1),
        json!(u64::from(u32::MAX) + 1),
        json!(0.5),
        json!("0"),
        Value::Null,
    ] {
        let mut invalid = wire.clone();
        invalid["delivery"]["job"]["operation"]["generation"] = bad;
        refuses::<Settlement>(invalid);
    }
    let mut maximum = wire.clone();
    maximum["delivery"]["job"]["operation"]["generation"] = json!(u32::MAX);
    round_trip(&serde_json::from_value::<Settlement>(maximum).unwrap());
    for path in [
        "/delivery/deadline",
        "/delivery/job/availableAt",
        "/successors/0/availableAt",
    ] {
        for bad in [
            json!(-1),
            json!(u64::MAX),
            json!(0.5),
            json!("0"),
            Value::Null,
        ] {
            let mut invalid = wire.clone();
            *invalid.pointer_mut(path).unwrap() = bad;
            refuses::<Settlement>(invalid);
        }
        for valid in [0, i64::MAX] {
            let mut boundary = wire.clone();
            *boundary.pointer_mut(path).unwrap() = json!(valid);
            round_trip(&serde_json::from_value::<Settlement>(boundary).unwrap());
        }
    }
}

#[test]
fn missing_fields_and_unknown_operations_or_outcomes_are_rejected() {
    for (operation, _) in operations() {
        let wire = round_trip(&settlement(operation));
        for path in ["", "/delivery", "/delivery/job", "/delivery/job/operation"] {
            let fields = wire.pointer(path).unwrap().as_object().unwrap();
            assert!(!fields.is_empty());
            for field in fields.keys() {
                let mut missing = wire.clone();
                missing
                    .pointer_mut(path)
                    .unwrap()
                    .as_object_mut()
                    .unwrap()
                    .remove(field);
                refuses::<Settlement>(missing);
            }
        }
    }
    let wire = round_trip(&settlement(JobOperation::Reconcile {}));
    for operation in [
        json!({"kind":"execute","body":{}}),
        json!({"kind":"Collect"}),
        json!("collect"),
        Value::Null,
    ] {
        let mut invalid = wire.clone();
        invalid["delivery"]["job"]["operation"] = operation;
        refuses::<Settlement>(invalid);
    }
    for outcome in [
        json!("failed"),
        json!("Completed"),
        json!({"completed":{"result":1}}),
        json!({"kind":"completed","body":{}}),
        Value::Null,
    ] {
        let mut invalid = wire.clone();
        invalid["outcome"] = outcome.clone();
        refuses::<Settlement>(invalid);
        refuses::<JobOutcome>(outcome);
    }
    for successors in [Value::Null, json!({}), json!("pending")] {
        let mut invalid = wire.clone();
        invalid["successors"] = successors;
        refuses::<Settlement>(invalid);
    }
}
