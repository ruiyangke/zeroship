use serde::{de::DeserializeOwned, Serialize};
use serde_json::{json, Value};
use std::fmt::Debug;
use zeroship_core::{
    app_id::AppId,
    workflow_coordination::{
        AssignedScope, InvalidRestart, ManagementOutcome, ManagementReceipt, RequestId,
        RestartDeploy, RestartOptions, RestartTarget, RunId, RunOperation, RunState, ScopePage,
        WorkerId,
    },
    workflow_jobs::{
        BroadcastId, Delivery, DeliveryLease, DeploymentId, JobId, JobOperation, JobOutcome,
        JobSpec, ManagementCommand, PropagationId, Settlement, SettlementReceipt, SubmitJob,
    },
    workflow_schedules::ScheduleId,
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

/// Drop each key of each named object in turn and require the decode to fail.
///
/// `round_trip` is the control: the untouched wire has to decode back to the
/// same value, so a shape the sweep could never have accepted cannot pass by
/// being refused for an unrelated reason.
fn requires_every_key<T: Debug + PartialEq + Serialize + DeserializeOwned>(
    value: &T,
    paths: &[&str],
) {
    let wire = round_trip(value);
    for path in paths {
        let fields = wire.pointer(path).unwrap().as_object().unwrap();
        assert!(!fields.is_empty(), "no keys to drop at {path:?}");
        for field in fields.keys() {
            let mut missing = wire.clone();
            missing
                .pointer_mut(path)
                .unwrap()
                .as_object_mut()
                .unwrap()
                .remove(field);
            refuses::<T>(missing);
        }
    }
}

/// Require a field carrying `None` to spell that null on the wire, and to
/// decode back from it unchanged.
///
/// `round_trip` alone passes over a field serde omitted, and an omitted key is
/// exactly the shape `requires_every_key` has to be able to see.
fn spells_its_null<T: Debug + PartialEq + Serialize + DeserializeOwned>(value: &T, path: &str) {
    assert_eq!(*round_trip(value).pointer(path).unwrap(), Value::Null);
}

fn operations() -> Vec<(JobOperation, Value)> {
    let run = RunId::mint();
    let request = RequestId::mint();
    let schedule = ScheduleId::mint();
    let deployment = DeploymentId::mint();
    let broadcast = BroadcastId::mint();
    let propagation = PropagationId::mint();
    let mut operations = vec![
        (
            JobOperation::Activate {
                deployment_id: deployment.clone(),
                revision: 1.try_into().unwrap(),
            },
            json!({"kind":"activate","deploymentId":deployment,"revision":1}),
        ),
        (
            JobOperation::Advance {
                deployment_id: deployment.clone(),
                run_id: run.clone(),
                generation: 0,
                revision: 1.try_into().unwrap(),
            },
            json!({"kind":"advance","deploymentId":deployment,"runId":run,"generation":0,"revision":1}),
        ),
        (
            JobOperation::Cron {
                deployment_id: deployment.clone(),
                schedule_id: schedule.clone(),
                schedule_name: "daily-report".into(),
                request_id: request.clone(),
                run_id: run.clone(),
                revision: 2.try_into().unwrap(),
                scheduled_at: 123.try_into().unwrap(),
            },
            json!({"kind":"cron","deploymentId":deployment,"scheduleId":schedule,"scheduleName":"daily-report","requestId":request,"runId":run,"revision":2,"scheduledAt":123}),
        ),
        (
            JobOperation::Fanout {
                broadcast_id: broadcast.clone(),
                revision: 1.try_into().unwrap(),
            },
            json!({"kind":"fanout", "broadcastId":broadcast, "revision":1}),
        ),
        (
            JobOperation::Propagate {
                propagation_id: propagation.clone(),
                revision: 1.try_into().unwrap(),
            },
            json!({"kind":"propagate", "propagationId":propagation, "revision":1}),
        ),
        (
            JobOperation::Close {
                epoch: 3.try_into().unwrap(),
            },
            json!({"kind":"close", "epoch":3}),
        ),
        (JobOperation::Reconcile {}, json!({"kind":"reconcile"})),
        (JobOperation::Collect {}, json!({"kind":"collect"})),
    ];
    for (command, expected) in [
        (
            ManagementCommand::Transition {
                operation: RunOperation::Pause,
            },
            json!({"kind":"transition","operation":"pause"}),
        ),
        (
            ManagementCommand::RestartStarted { from: None },
            json!({"kind":"restart_started"}),
        ),
        (
            ManagementCommand::RestartStarted {
                from: Some(RestartTarget {
                    name: "charge".into(),
                    occurrence: Some(0),
                }),
            },
            json!({"kind":"restart_started","from":{"name":"charge","occurrence":0}}),
        ),
        (
            ManagementCommand::RestartLatest {
                deployment_id: deployment.clone(),
            },
            json!({"kind":"restart_latest","deploymentId":deployment}),
        ),
    ] {
        operations.push((
            JobOperation::Management {
                request_id: request.clone(), run_id: run.clone(),
                revision: 1.try_into().unwrap(), command,
            },
            json!({"kind":"management","requestId":request,"runId":run,"revision":1,"command":expected}),
        ));
    }
    operations
}

/// The applied result a command asks for, so a fixture settlement answers the
/// command it carries rather than standing in for any of them.
fn management_result(command: &ManagementCommand) -> ManagementOutcome {
    match command {
        ManagementCommand::Transition { .. } => ManagementOutcome::Applied {
            state: RunState::Paused,
        },
        ManagementCommand::RestartStarted { .. } | ManagementCommand::RestartLatest { .. } => {
            ManagementOutcome::Restarted {
                state: RunState::Queued,
                restarted_from_ordinal: Some(2),
                pinned_to: DeploymentId::mint(),
            }
        }
    }
}

fn settlement(operation: JobOperation) -> Settlement {
    let outcome = match &operation {
        JobOperation::Management { command, .. } => JobOutcome::Management {
            outcome: management_result(command),
        },
        JobOperation::Close { .. } => JobOutcome::Closed { drained: true },
        _ => JobOutcome::Waiting {},
    };
    let job = JobSpec {
        id: JobId::mint(),
        app_id: AppId::mint(),
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
        outcome,
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
}

fn outcomes() -> Vec<(JobOutcome, Value)> {
    let mut cases = vec![
        (JobOutcome::Completed {}, json!({"kind":"completed"})),
        (JobOutcome::Waiting {}, json!({"kind":"waiting"})),
        (JobOutcome::Rejected {}, json!({"kind":"rejected"})),
        (
            JobOutcome::Closed { drained: true },
            json!({"kind":"closed","drained":true}),
        ),
        (
            JobOutcome::Closed { drained: false },
            json!({"kind":"closed","drained":false}),
        ),
    ];
    let pinned_to = DeploymentId::mint();
    for (outcome, wire) in [
        (
            ManagementOutcome::Applied {
                state: RunState::Paused,
            },
            json!({"kind":"applied","state":"paused"}),
        ),
        (
            ManagementOutcome::Restarted {
                state: RunState::Queued,
                restarted_from_ordinal: Some(7),
                pinned_to: pinned_to.clone(),
            },
            json!({"kind":"restarted","state":"queued","restartedFromOrdinal":7,
                "pinnedTo":pinned_to}),
        ),
        (
            ManagementOutcome::Restarted {
                state: RunState::Queued,
                restarted_from_ordinal: None,
                pinned_to: pinned_to.clone(),
            },
            json!({"kind":"restarted","state":"queued","restartedFromOrdinal":null,
                "pinnedTo":pinned_to}),
        ),
        (ManagementOutcome::NotFound {}, json!({"kind":"not_found"})),
        (ManagementOutcome::Conflict {}, json!({"kind":"conflict"})),
        (ManagementOutcome::Denied {}, json!({"kind":"denied"})),
    ] {
        cases.push((
            JobOutcome::Management { outcome },
            json!({"kind":"management","outcome":wire}),
        ));
    }
    cases
}

/// Management and closure each own a private outcome family; every other
/// operation shares the ordinary scheduling outcomes.
const fn operation_family(operation: &JobOperation) -> u8 {
    match operation {
        JobOperation::Management { .. } => 1,
        JobOperation::Close { .. } => 2,
        _ => 0,
    }
}

const fn outcome_family(outcome: &JobOutcome) -> u8 {
    match outcome {
        JobOutcome::Management { .. } => 1,
        JobOutcome::Closed { .. } => 2,
        _ => 0,
    }
}

/// Whether a management result answers the command that asked for it. Sharing
/// the management family is necessary but not sufficient: the applied arms are
/// command-shaped, while the refusals answer any command.
const fn command_answered(operation: &JobOperation, outcome: &JobOutcome) -> bool {
    let (
        JobOperation::Management { command, .. },
        JobOutcome::Management {
            outcome: management,
        },
    ) = (operation, outcome)
    else {
        return true;
    };
    match (command, management) {
        (ManagementCommand::Transition { .. }, ManagementOutcome::Applied { .. })
        | (
            ManagementCommand::RestartStarted { .. } | ManagementCommand::RestartLatest { .. },
            ManagementOutcome::Restarted { .. },
        )
        | (
            _,
            ManagementOutcome::NotFound {}
            | ManagementOutcome::Conflict {}
            | ManagementOutcome::Denied {},
        ) => true,
        (ManagementCommand::Transition { .. }, ManagementOutcome::Restarted { .. })
        | (
            ManagementCommand::RestartStarted { .. } | ManagementCommand::RestartLatest { .. },
            ManagementOutcome::Applied { .. },
        ) => false,
    }
}

#[test]
fn outcome_objects_preserve_closed_management_results_and_operation_families() {
    let mut families = std::collections::BTreeSet::new();
    for (outcome, wire) in outcomes() {
        assert_eq!(round_trip(&outcome), wire);
        for (operation, _) in operations() {
            let expected = operation_family(&operation) == outcome_family(&outcome)
                && command_answered(&operation, &outcome);
            if expected {
                families.insert(outcome_family(&outcome));
            }
            assert_eq!(
                outcome.valid_for(&operation),
                expected,
                "{operation:?}: {outcome:?}"
            );
            if expected {
                let mut command = settlement(operation);
                command.outcome = outcome.clone();
                let encoded = round_trip(&command);
                assert_eq!(encoded["outcome"], wire);
                let receipt = SettlementReceipt {
                    app_id: command.delivery.job.app_id,
                    job_id: command.delivery.job.id,
                    attempt: command.delivery.attempt,
                    outcome: outcome.clone(),
                };
                assert_eq!(round_trip(&receipt)["outcome"], wire);
            }
        }
    }
    assert_eq!(
        families.into_iter().collect::<Vec<_>>(),
        [0, 1, 2],
        "every outcome family must meet a valid operation"
    );
}

/// A management result must answer the command that asked for it.
///
/// Both directions are asserted, because either alone is passed by a degenerate
/// check: a refusal-only test passes a `valid_for` that answers `false` for
/// everything, and an acceptance-only test passes the family check this
/// replaces. The two controls below pin the table to both verdicts, so a table
/// that drifted to one of them fails before the pairing is exercised.
///
/// What this does not catch: `valid_for` is a check a caller applies, not a
/// constraint on the type, so nothing here stops a mismatched
/// `JobOutcome::Management` being constructed, serialized or stored; only the
/// sites that call it refuse one. It says nothing about which commands or
/// states the creator lifecycle allows, whether the command was authorized, or
/// whether the transaction that produced the outcome committed. It also does
/// not bind the restart arms apart: both restart commands accept the same
/// result, so it cannot tell a started restart's pin from a latest restart's.
#[test]
fn management_outcomes_pair_with_the_command_that_asked_for_them() {
    let applied = ManagementOutcome::Applied {
        state: RunState::Cancelled,
    };
    let restarted = ManagementOutcome::Restarted {
        state: RunState::Queued,
        restarted_from_ordinal: Some(3),
        pinned_to: DeploymentId::mint(),
    };
    let mut cases = Vec::new();
    for (command, applied_answers, restarted_answers) in [
        (
            ManagementCommand::Transition {
                operation: RunOperation::Cancel,
            },
            true,
            false,
        ),
        (
            ManagementCommand::RestartStarted {
                from: Some(RestartTarget {
                    name: "charge".into(),
                    occurrence: Some(2),
                }),
            },
            false,
            true,
        ),
        (ManagementCommand::RestartStarted { from: None }, false, true),
        (
            ManagementCommand::RestartLatest {
                deployment_id: DeploymentId::mint(),
            },
            false,
            true,
        ),
    ] {
        cases.push((command.clone(), applied.clone(), applied_answers));
        cases.push((command.clone(), restarted.clone(), restarted_answers));
        // A refusal is the same refusal whichever command was asked, and
        // nothing it carries is command-shaped.
        for refusal in [
            ManagementOutcome::NotFound {},
            ManagementOutcome::Conflict {},
            ManagementOutcome::Denied {},
        ] {
            cases.push((command.clone(), refusal, true));
        }
    }
    assert!(
        cases.iter().any(|(.., valid)| *valid),
        "the table must accept some pairing"
    );
    assert!(
        cases.iter().any(|(.., valid)| !*valid),
        "the table must refuse some pairing"
    );
    for (command, outcome, valid) in cases {
        let operation = JobOperation::Management {
            request_id: RequestId::mint(),
            run_id: RunId::mint(),
            revision: 1.try_into().unwrap(),
            command,
        };
        let settled = JobOutcome::Management {
            outcome: outcome.clone(),
        };
        assert_eq!(
            settled.valid_for(&operation),
            valid,
            "{operation:?}: {outcome:?}"
        );
        // The pairing narrows the management family; it does not widen it.
        assert!(!settled.valid_for(&JobOperation::Reconcile {}), "{outcome:?}");
        assert!(
            !settled.valid_for(&JobOperation::Close {
                epoch: 1.try_into().unwrap(),
            }),
            "{outcome:?}"
        );
        for other in [
            JobOutcome::Completed {},
            JobOutcome::Waiting {},
            JobOutcome::Rejected {},
            JobOutcome::Closed { drained: true },
        ] {
            assert!(!other.valid_for(&operation), "{other:?}");
        }
    }
}

#[test]
fn outcome_objects_reject_private_data_missing_fields_and_secondary_results() {
    for (outcome, wire) in outcomes() {
        let command = settlement(JobOperation::Reconcile {});
        let receipt = SettlementReceipt {
            app_id: command.delivery.job.app_id.clone(),
            job_id: command.delivery.job.id.clone(),
            attempt: command.delivery.attempt,
            outcome: outcome.clone(),
        };
        let mut paths = vec![""];
        if matches!(outcome, JobOutcome::Management { .. }) {
            paths.push("/outcome");
        }
        for path in paths {
            for field in [
                "input",
                "history",
                "result",
                "error",
                "body",
                "credentials",
                "payloadUrl",
            ] {
                let mut invalid = wire.clone();
                invalid
                    .pointer_mut(path)
                    .unwrap()
                    .as_object_mut()
                    .unwrap()
                    .insert(field.into(), json!({"private":"data"}));
                refuses::<JobOutcome>(invalid.clone());
                let mut invalid_command = json!(command);
                invalid_command["outcome"] = invalid.clone();
                refuses::<Settlement>(invalid_command);
                let mut invalid_receipt = json!(receipt);
                invalid_receipt["outcome"] = invalid;
                refuses::<SettlementReceipt>(invalid_receipt);
            }
            let object = wire.pointer(path).unwrap().as_object().unwrap();
            for field in object.keys() {
                let mut missing = wire.clone();
                missing
                    .pointer_mut(path)
                    .unwrap()
                    .as_object_mut()
                    .unwrap()
                    .remove(field);
                refuses::<JobOutcome>(missing);
            }
        }
        let mut secondary = json!(receipt);
        secondary["managementOutcome"] = json!({"kind":"denied"});
        refuses::<SettlementReceipt>(secondary);
        let mut secondary = json!(command);
        secondary["managementOutcome"] = json!({"kind":"denied"});
        refuses::<Settlement>(secondary);
        if !matches!(outcome, JobOutcome::Management { .. }) {
            let mut foreign = wire;
            foreign["outcome"] = json!({"kind":"denied"});
            refuses::<JobOutcome>(foreign);
        }
    }
    for nested in [
        Value::Null,
        json!("denied"),
        json!({"kind":"unknown"}),
        json!({"kind":"applied"}),
        json!({"kind":"applied","state":"invented"}),
        json!({"kind":"applied","state":null}),
        json!({"kind":"denied","state":"paused"}),
    ] {
        refuses::<JobOutcome>(json!({"kind":"management","outcome":nested}));
    }
}

#[test]
fn executable_prerequisites_belong_only_to_operations_that_require_code() {
    let mut executable = false;
    let mut journal_only = false;
    for (operation, expected) in operations() {
        let job = settlement(operation).delivery.job;
        let wire = round_trip(&job);
        assert!(wire.get("deploymentId").is_none());
        let expected_deployment = expected
            .get("deploymentId")
            .or_else(|| expected.pointer("/command/deploymentId"));
        assert_eq!(
            job.deployment_id().map(|id| json!(id)),
            expected_deployment.cloned(),
        );
        let mut obsolete = wire.clone();
        obsolete["deploymentId"] = json!(DeploymentId::mint());
        refuses::<JobSpec>(obsolete);
        let prerequisite_path = if matches!(job.operation, JobOperation::Management { .. }) {
            "/operation/command"
        } else {
            "/operation"
        };
        if job.deployment_id().is_some() {
            executable = true;
            let mut missing = wire.clone();
            missing
                .pointer_mut(prerequisite_path)
                .unwrap()
                .as_object_mut()
                .unwrap()
                .remove("deploymentId");
            refuses::<JobSpec>(missing);
            let mut null = wire;
            null.pointer_mut(prerequisite_path).unwrap()["deploymentId"] = Value::Null;
            refuses::<JobSpec>(null);
        } else {
            journal_only = true;
            let mut injected = wire;
            injected.pointer_mut(prerequisite_path).unwrap()["deploymentId"] =
                json!(DeploymentId::mint());
            refuses::<JobSpec>(injected);
        }
    }
    assert!(executable && journal_only);
}

#[test]
fn management_commands_reject_ambiguous_targets_and_extra_authority() {
    let mut saw_management = false;
    for (operation, _) in operations() {
        let JobOperation::Management { command, .. } = operation else {
            continue;
        };
        saw_management = true;
        let wire = round_trip(&command);
        for field in ["input", "history", "payloadUrl", "credentials", "deploy"] {
            let mut injected = wire.clone();
            injected[field] = json!({"untrusted":true});
            refuses::<ManagementCommand>(injected);
        }
        if let ManagementCommand::RestartLatest { .. } = command {
            for from in [Value::Null, json!({"name":"charge"})] {
                let mut partial = wire.clone();
                partial["from"] = from;
                refuses::<ManagementCommand>(partial);
            }
        }
        if let ManagementCommand::RestartStarted { from: Some(_) } = command {
            let mut injected = wire;
            injected["from"]["input"] = json!({"private":true});
            refuses::<ManagementCommand>(injected);
        }
    }
    assert!(saw_management);
    for command in [
        json!({"kind":"restart"}),
        json!({"kind":"transition"}),
        json!({"kind":"restart_latest"}),
        json!({"kind":"restart_latest","deploymentId":RunId::mint()}),
        json!({"kind":"restart_started","from":{"name":"charge","occurrence":-1}}),
    ] {
        refuses::<ManagementCommand>(command);
    }
}

#[test]
fn restart_policy_is_normalized_before_resolving_the_executable_prerequisite() {
    for deploy in [
        None,
        Some(RestartDeploy::Started),
        Some(RestartDeploy::Latest),
    ] {
        let full = RestartOptions { from: None, deploy };
        assert_eq!(
            full.effective_deploy(),
            Ok(deploy.unwrap_or(RestartDeploy::Latest))
        );
        let partial = RestartOptions {
            from: Some(RestartTarget {
                name: "charge".into(),
                occurrence: None,
            }),
            deploy,
        };
        assert_eq!(
            partial.effective_deploy(),
            if deploy == Some(RestartDeploy::Latest) {
                Err(InvalidRestart::PartialLatest)
            } else {
                Ok(RestartDeploy::Started)
            }
        );
    }
    let mut options = RestartOptions {
        from: Some(RestartTarget {
            name: String::new(),
            occurrence: None,
        }),
        deploy: None,
    };
    assert_eq!(
        options.effective_deploy(),
        Err(InvalidRestart::EmptyTargetName)
    );
    let target = options.from.as_mut().unwrap();
    target.name = "charge".into();
    target.occurrence = Some(i32::MAX as u32 + 1);
    assert_eq!(
        options.effective_deploy(),
        Err(InvalidRestart::TargetOccurrenceOutOfRange)
    );
    options.from.as_mut().unwrap().occurrence = Some(i32::MAX as u32);
    assert_eq!(options.effective_deploy(), Ok(RestartDeploy::Started));
}

#[test]
fn delivery_and_settlement_preserve_logical_and_attempt_identities() {
    let settlement = settlement(JobOperation::Collect {});
    let delivery = &settlement.delivery;
    let job = &delivery.job;
    let expected_job = json!({
        "id":job.id,"appId":job.app_id,
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
            "delivery":expected_delivery,"outcome":{"kind":"waiting"},
            "successors":[{
                "id":successor.id,"appId":job.app_id,
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
        json!({"jobId":job.id,"appId":job.app_id,"attempt":3,"outcome":{"kind":"waiting"}})
    );
    round_trip(&Settlement {
        successors: Vec::new(),
        outcome: JobOutcome::Completed {},
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
            "/outcome",
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
        outcome: JobOutcome::Rejected {},
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
            ("/delivery/job/operation/deploymentId", json!(JobId::mint())),
            (
                "/delivery/job/operation/command/deploymentId",
                json!(JobId::mint()),
            ),
            ("/delivery/job/appId", json!(WorkerId::mint())),
            ("/delivery/workerId", json!(AppId::mint())),
            ("/delivery/job/operation/runId", json!(RequestId::mint())),
            ("/delivery/job/operation/requestId", json!(RunId::mint())),
            ("/delivery/job/operation/scheduleId", json!(RunId::mint())),
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
        deployment_id: DeploymentId::mint(),
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
fn workflow_operations_require_native_revisions_and_cron_identity() {
    let cases: Vec<_> = operations()
        .into_iter()
        .filter(|(operation, _)| {
            matches!(
                operation,
                JobOperation::Activate { .. }
                    | JobOperation::Cron { .. }
                    | JobOperation::Management { .. }
                    | JobOperation::Fanout { .. }
                    | JobOperation::Propagate { .. }
            )
        })
        .collect();
    assert!(!cases.is_empty());
    for (operation, _) in cases {
        let wire = round_trip(&operation);
        for bad in [
            json!(0),
            json!(-1),
            json!(u64::MAX),
            json!(1.5),
            json!("1"),
            Value::Null,
        ] {
            let mut invalid = wire.clone();
            invalid["revision"] = bad;
            refuses::<JobOperation>(invalid);
        }
        let mut maximum = wire.clone();
        maximum["revision"] = json!(i64::MAX);
        round_trip(&serde_json::from_value::<JobOperation>(maximum).unwrap());
        if !matches!(operation, JobOperation::Cron { .. }) {
            continue;
        }
        for bad in [json!("sch_"), json!(RunId::mint()), json!(1), Value::Null] {
            let mut invalid = wire.clone();
            invalid["scheduleId"] = bad;
            refuses::<JobOperation>(invalid);
        }
        for bad in [json!({"input":"private"}), json!(1), Value::Null] {
            let mut invalid = wire.clone();
            invalid["scheduleName"] = bad;
            refuses::<JobOperation>(invalid);
        }
        for bad in [
            json!(-1),
            json!(u64::MAX),
            json!(0.5),
            json!("0"),
            Value::Null,
        ] {
            let mut invalid = wire.clone();
            invalid["scheduledAt"] = bad;
            refuses::<JobOperation>(invalid);
        }
        for valid in [0, i64::MAX] {
            let mut boundary = wire.clone();
            boundary["scheduledAt"] = json!(valid);
            round_trip(&serde_json::from_value::<JobOperation>(boundary).unwrap());
        }
    }
}

/// Every key a coordination envelope declares has to be on the wire.
///
/// A bare `Option` field decodes a dropped key as `None`, which is a value
/// each of these types gives its own meaning: an absent `after` scans from the
/// first app instead of the page that was asked for, and an absent `outcome`
/// reports a settled command as one the manager has not applied yet. Each
/// nullable field is swept from both sides, because a change that refused the
/// explicit null as well would satisfy a refusal-only sweep while breaking
/// every producer.
///
/// What this does not catch: it walks the types named here, so a coordination
/// type nobody added to it keeps the tolerance unobserved. It says nothing
/// about the fields that are deliberately absence-tolerant, the ones carrying
/// `skip_serializing_if`, whose omission is how they spell `None`. It binds
/// the encoding only; it cannot tell whether a producer computed the right
/// cursor or the right outcome, nor whether a transport preserved the key
/// between them.
#[test]
fn missing_fields_and_unknown_operations_or_outcomes_are_rejected() {
    for (operation, _) in operations() {
        requires_every_key(
            &settlement(operation),
            &["", "/delivery", "/delivery/job", "/delivery/job/operation"],
        );
    }
    let app = AppId::mint();
    let request = RequestId::mint();
    let page = ScopePage { after: None };
    spells_its_null(&page, "/after");
    requires_every_key(&page, &[""]);
    requires_every_key(
        &ScopePage {
            after: Some(app.clone()),
        },
        &[""],
    );
    let pending = ManagementReceipt {
        app_id: app.clone(),
        request_id: request.clone(),
        outcome: None,
    };
    spells_its_null(&pending, "/outcome");
    requires_every_key(&pending, &[""]);
    for outcome in [
        ManagementOutcome::Applied {
            state: RunState::Paused,
        },
        ManagementOutcome::Restarted {
            state: RunState::Queued,
            restarted_from_ordinal: None,
            pinned_to: DeploymentId::mint(),
        },
        ManagementOutcome::Denied {},
    ] {
        requires_every_key(
            &ManagementReceipt {
                app_id: app.clone(),
                request_id: request.clone(),
                outcome: Some(outcome),
            },
            &["", "/outcome"],
        );
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
        json!("completed"),
        json!("waiting"),
        json!("rejected"),
        json!("management"),
        json!("applied"),
        json!("not_found"),
        json!("conflict"),
        json!("denied"),
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

#[test]
fn fanout_requires_broadcast_identity_and_rejects_customer_routing_state() {
    let operation = JobOperation::Fanout {
        broadcast_id: BroadcastId::mint(),
        revision: 1.try_into().unwrap(),
    };
    let wire = round_trip(&operation);
    for bad in [
        Value::Null,
        json!(1),
        json!("wbc_"),
        json!(JobId::mint()),
        json!(RunId::mint()),
    ] {
        let mut invalid = wire.clone();
        invalid["broadcastId"] = bad;
        refuses::<JobOperation>(invalid);
    }
    for field in [
        "topic",
        "cursor",
        "cutoff",
        "recipients",
        "subscriptions",
        "body",
        "deploymentId",
        "runId",
        "generation",
    ] {
        let mut invalid = wire.clone();
        invalid[field] = json!("private");
        refuses::<JobOperation>(invalid);
    }
}

#[test]
fn propagate_names_only_an_opaque_obligation_page() {
    let obligation = PropagationId::mint();
    let operation = JobOperation::Propagate {
        propagation_id: obligation.clone(),
        revision: 2.try_into().unwrap(),
    };
    let wire = round_trip(&operation);
    assert_eq!(
        wire,
        json!({"kind":"propagate", "propagationId":obligation, "revision":2})
    );
    let job = JobSpec {
        id: JobId::mint(),
        app_id: AppId::mint(),
        operation,
        available_at: 0.try_into().unwrap(),
    };
    assert_eq!(job.deployment_id(), None);
    for outcome in [JobOutcome::Completed {}, JobOutcome::Waiting {}] {
        assert!(outcome.valid_for(&job.operation));
    }
    assert!(!JobOutcome::Management {
        outcome: ManagementOutcome::NotFound {},
    }
    .valid_for(&job.operation));
    for bad in [
        Value::Null,
        json!(1),
        json!("wdp_"),
        json!(BroadcastId::mint()),
        json!(RunId::mint()),
        json!(JobId::mint()),
    ] {
        let mut invalid = wire.clone();
        invalid["propagationId"] = bad;
        refuses::<JobOperation>(invalid);
    }
    for field in [
        "kindOfObligation",
        "cascade",
        "notify",
        "cursor",
        "runId",
        "generation",
        "headId",
        "parents",
        "children",
        "deploymentId",
        "broadcastId",
    ] {
        let mut invalid = wire.clone();
        invalid[field] = json!("private");
        refuses::<JobOperation>(invalid);
    }
    let mut missing = wire;
    missing.as_object_mut().unwrap().remove("propagationId");
    refuses::<JobOperation>(missing);
}

#[test]
fn close_names_only_an_epoch_and_owns_the_closed_evidence_outcome() {
    let operation = JobOperation::Close {
        epoch: 7.try_into().unwrap(),
    };
    let wire = round_trip(&operation);
    assert_eq!(wire, json!({"kind":"close", "epoch":7}));
    let job = JobSpec {
        id: JobId::mint(),
        app_id: AppId::mint(),
        operation,
        available_at: 0.try_into().unwrap(),
    };
    assert_eq!(job.deployment_id(), None);
    assert!(!job.produces_intents());
    for drained in [false, true] {
        assert!(JobOutcome::Closed { drained }.valid_for(&job.operation));
    }
    for outcome in [
        JobOutcome::Completed {},
        JobOutcome::Waiting {},
        JobOutcome::Rejected {},
        JobOutcome::Management {
            outcome: ManagementOutcome::NotFound {},
        },
    ] {
        assert!(!outcome.valid_for(&job.operation), "{outcome:?}");
    }
    for bad in [
        json!(0),
        json!(-1),
        json!(u64::MAX),
        json!(1.5),
        json!("1"),
        Value::Null,
    ] {
        let mut invalid = wire.clone();
        invalid["epoch"] = bad;
        refuses::<JobOperation>(invalid);
    }
    let mut maximum = wire.clone();
    maximum["epoch"] = json!(i64::MAX);
    round_trip(&serde_json::from_value::<JobOperation>(maximum).unwrap());
    for field in [
        "drained",
        "watermark",
        "state",
        "closedEpoch",
        "intents",
        "deploymentId",
        "runId",
    ] {
        let mut invalid = wire.clone();
        invalid[field] = json!("private");
        refuses::<JobOperation>(invalid);
    }
    let mut missing = wire;
    missing.as_object_mut().unwrap().remove("epoch");
    refuses::<JobOperation>(missing);
    for bad in [
        json!({"kind":"closed"}),
        json!({"kind":"closed","drained":null}),
        json!({"kind":"closed","drained":"true"}),
        json!({"kind":"closed","drained":1}),
        json!({"kind":"closed","drained":true,"pending":3}),
    ] {
        refuses::<JobOutcome>(bad);
    }
}

#[test]
fn only_intent_producing_operations_can_re_establish_responsibility() {
    let mut producing = 0;
    let mut maintenance = 0;
    for (operation, _) in operations() {
        let expected = !matches!(
            operation,
            JobOperation::Close { .. } | JobOperation::Reconcile {} | JobOperation::Collect {}
        );
        let job = settlement(operation).delivery.job;
        assert_eq!(job.produces_intents(), expected, "{:?}", job.operation);
        if expected {
            producing += 1;
        } else {
            maintenance += 1;
        }
    }
    assert!(producing > 0 && maintenance > 0);
}

/// `RunState::ALL` is what every other check here enumerates, so a variant
/// missing from it would take all of them past a state nothing covers.
///
/// `position` is an exhaustive match, so the compiler refuses a variant with no
/// arm; reading `ALL[position]` back is what refuses an arm pointing anywhere
/// but the slot that holds the variant, and `ALL` cannot offer such a slot
/// without listing it. The two together are what make the array the variant set
/// rather than a hand-list beside it.
#[test]
fn every_run_state_occupies_the_slot_it_names() {
    for state in RunState::ALL {
        assert_eq!(
            RunState::ALL[state.position()],
            state,
            "{} does not occupy the slot it names",
            state.as_str()
        );
    }
    let mut names = RunState::ALL.map(RunState::as_str).to_vec();
    names.sort_unstable();
    let distinct = names.len();
    names.dedup();
    assert_eq!(names.len(), distinct, "two run states share a name");
}

/// Queries that select live runs by state string read `RunState::TERMINAL`, and
/// code that decides whether a run is at rest reads `is_terminal`. A state in
/// only one of them is a run the platform stops dispatching but keeps counting
/// as live, so the two must enumerate the same set.
#[test]
fn the_terminal_state_names_and_the_terminal_predicate_agree() {
    for name in RunState::TERMINAL {
        assert!(
            RunState::ALL.iter().any(|state| state.as_str() == name),
            "terminal name is not a run state: {name}"
        );
    }
    let mut terminal = 0;
    let mut live = 0;
    for state in RunState::ALL {
        let named = RunState::TERMINAL.contains(&state.as_str());
        assert_eq!(named, state.is_terminal(), "{}", state.as_str());
        assert_eq!(state.as_str().parse::<RunState>().unwrap(), state);
        assert_eq!(
            serde_json::to_value(state).unwrap(),
            serde_json::Value::String(state.as_str().to_owned()),
            "the serialized name differs from as_str",
        );
        if state.is_terminal() {
            terminal += 1;
        } else {
            live += 1;
        }
    }
    assert_eq!(terminal, RunState::TERMINAL.len());
    assert!(live > 0);
}

/// A run that continued as new is at rest: its work moved to the successor, so
/// nothing further dispatches it and a live-run query must not return it.
#[test]
fn a_continued_run_is_terminal_and_names_itself_distinctly() {
    assert!(RunState::ContinuedAsNew.is_terminal());
    assert!(RunState::TERMINAL.contains(&RunState::ContinuedAsNew.as_str()));
    assert_ne!(
        RunState::ContinuedAsNew.as_str(),
        RunState::Completed.as_str()
    );
    assert_eq!(
        "continuedAsNew".parse::<RunState>().unwrap(),
        RunState::ContinuedAsNew
    );
}
