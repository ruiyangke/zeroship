use serde::{de::DeserializeOwned, Serialize};
use serde_json::{json, Value};
use std::fmt::Debug;
use zeroship_core::{
    app_id::AppId,
    workflow_coordination::{
        AssignedScope, Assignment, DeploymentId, Failure, ManageRun, ManagementOperation,
        ManagementOutcome, ManagementReceipt, ManagementStatus, RegisterWorker, RegisteredWorker,
        ReleaseScope, RequestId, Revision, RunId, ScopePage, UnixMillis, VerifyAssignment,
        WorkerId,
    },
};

fn rejects_customer_fields<T: DeserializeOwned + Serialize + PartialEq + Debug>(value: &Value) {
    let decoded: T = serde_json::from_value(value.clone()).expect("valid metadata control");
    let encoded = serde_json::to_value(&decoded).unwrap();
    assert_eq!(serde_json::from_value::<T>(encoded).unwrap(), decoded);
    for field in [
        "input",
        "output",
        "error",
        "history",
        "journal",
        "payload",
        "outputRef",
        "taskToken",
        "databaseUrl",
        "schema",
        "storageCredentials",
        "payloadUrl",
    ] {
        let mut attempted = value.clone();
        attempted[field] = json!({"customer":"private"});
        assert!(
            serde_json::from_value::<T>(attempted).is_err(),
            "accepted {field}"
        );
    }
}

#[test]
fn registry_and_placement_contracts_reject_customer_data() {
    let app = AppId::mint();
    let worker = WorkerId::mint();
    let request = RequestId::mint();
    rejects_customer_fields::<ScopePage>(&json!({"after":app}));
    rejects_customer_fields::<ManagementStatus>(&json!({"appId":app,"requestId":request}));
    rejects_customer_fields::<Failure>(&json!({"code":"denied"}));
    rejects_customer_fields::<RegisterWorker>(&json!({"capacity":4,"state":"ready"}));
    rejects_customer_fields::<RegisteredWorker>(&json!({
        "workerId":worker,"capacity":4,"state":"ready","expiresAt":1000
    }));
    rejects_customer_fields::<Assignment>(&json!({
        "appId":app,"workerId":worker,"revision":1,"expiresAt":1000
    }));
    rejects_customer_fields::<VerifyAssignment>(&json!({
        "appId":app,"workerId":worker,"assignmentRevision":1
    }));
    let valid = json!({"appId":app,"workerId":worker,"assignmentRevision":1});
    for field in ["appId", "workerId", "assignmentRevision"] {
        let mut missing = valid.clone();
        missing.as_object_mut().unwrap().remove(field);
        assert!(serde_json::from_value::<VerifyAssignment>(missing).is_err());
    }
    for (field, invalid) in [
        ("appId", json!(worker)),
        ("workerId", json!(app)),
        ("assignmentRevision", json!(0)),
    ] {
        let mut malformed = valid.clone();
        malformed[field] = invalid;
        assert!(serde_json::from_value::<VerifyAssignment>(malformed).is_err());
    }
    rejects_customer_fields::<AssignedScope>(&json!({
        "appId":app,"assignmentRevision":1
    }));
    for reason in ["relinquished", "refused"] {
        rejects_customer_fields::<ReleaseScope>(&json!({
            "requestId":request,"appId":app,"assignmentRevision":1,"reason":reason
        }));
    }
    for reason in [json!(null), json!("drained"), json!(2)] {
        assert!(
            serde_json::from_value::<ReleaseScope>(json!({
                "requestId":request,"appId":app,"assignmentRevision":1,"reason":reason
            }))
            .is_err(),
            "release names one closed reason"
        );
    }
    assert!(
        serde_json::from_value::<ReleaseScope>(json!({
            "requestId":request,"appId":app,"assignmentRevision":1,"reason":"refused",
            "wakeRevision":2
        }))
        .is_err(),
        "release carries no wake hint"
    );
    for field in ["workerId", "appId"] {
        let mut attempted = json!({"capacity":4,"state":"ready"});
        attempted[field] = json!(worker);
        assert!(serde_json::from_value::<RegisterWorker>(attempted).is_err());
    }
}

#[test]
fn management_and_nested_receipts_cannot_carry_execution_data() {
    let app = AppId::mint();
    let request = RequestId::mint();
    rejects_customer_fields::<ManagementReceipt>(&json!({
        "appId":app,"requestId":request,"outcome":null
    }));
    let deployment = json!({"deploymentId":DeploymentId::mint(),
        "deployHash":"a".repeat(64)});
    for command in [
        json!({"kind":"transition","operation":"pause"}),
        json!({"kind":"restart","deployment":null,
            "options":{"from":{"name":"checkpoint","occurrence":0},"deploy":"started"}}),
        json!({"kind":"restart","deployment":deployment,
            "options":{"deploy":"latest"}}),
    ] {
        rejects_customer_fields::<ManagementOperation>(&command);
        let envelope =
            json!({"requestId":request,"appId":app,"runId":RunId::mint(),"command":command});
        rejects_customer_fields::<ManageRun>(&envelope);
        let mut attempted = envelope;
        attempted["command"]["input"] = json!("private");
        assert!(serde_json::from_value::<ManageRun>(attempted).is_err());
    }
    // `deployment` may be null but may never be absent: a producer that drops
    // the key must fail rather than read back as a started restart.
    assert!(serde_json::from_value::<ManagementOperation>(
        json!({"kind":"restart","options":{"deploy":"latest"}})
    )
    .is_err());
    // The named deployment carries an id and a hash, and nothing else.
    for field in ["appId", "manifestJson", "retentionState"] {
        let mut attempted = deployment.clone();
        attempted[field] = json!("private");
        assert!(
            serde_json::from_value::<ManagementOperation>(
                json!({"kind":"restart","deployment":attempted,
                    "options":{"deploy":"latest"}})
            )
            .is_err(),
            "accepted {field}"
        );
    }
    for outcome in [
        json!({"kind":"applied","state":"paused"}),
        json!({"kind":"restarted","state":"queued","restartedFromOrdinal":3,
            "pinnedTo":DeploymentId::mint()}),
        json!({"kind":"restarted","state":"queued","restartedFromOrdinal":null,
            "pinnedTo":DeploymentId::mint()}),
        json!({"kind":"not_found"}),
        json!({"kind":"conflict"}),
        json!({"kind":"denied"}),
    ] {
        rejects_customer_fields::<ManagementOutcome>(&outcome);
        rejects_customer_fields::<zeroship_core::workflow_jobs::JobOutcome>(
            &json!({"kind":"management","outcome":outcome}),
        );
    }
    for command in [
        json!({"kind":"start","input":"private"}),
        json!({"kind":"signal","payload":"private"}),
        json!({"kind":"complete","outcomes":[]}),
        json!({"kind":"restart","options":{"input":"private"}}),
        json!({"kind":"restart","options":{"from":{"name":"checkpoint","payload":"private"}}}),
    ] {
        assert!(serde_json::from_value::<ManagementOperation>(command).is_err());
    }
}

#[test]
fn metadata_ids_and_counters_validate_before_storage() {
    for invalid in [
        json!(0),
        json!(-1),
        json!(i64::MAX as u64 + 1),
        json!("1"),
        Value::Null,
    ] {
        assert!(serde_json::from_value::<Revision>(invalid).is_err());
    }
    for valid in [1, i64::MAX] {
        let revision = Revision::try_from(valid).unwrap();
        assert_eq!(serde_json::to_value(revision).unwrap(), json!(valid));
        assert_eq!(
            serde_json::from_value::<Revision>(json!(valid))
                .unwrap()
                .get(),
            valid
        );
    }
    assert!(UnixMillis::try_from(-1).is_err());
    assert_eq!(
        serde_json::to_value(UnixMillis::try_from(0).unwrap()).unwrap(),
        json!(0)
    );
    assert!(
        serde_json::from_value::<RegisterWorker>(json!({"capacity":0,"state":"ready"})).is_err()
    );
    let app = AppId::mint();
    assert!(serde_json::from_value::<WorkerId>(json!(app)).is_err());
    assert!(serde_json::from_value::<RunId>(json!(WorkerId::mint())).is_err());
    assert!(serde_json::from_value::<RequestId>(json!(RunId::mint())).is_err());
    let assignment = json!({"appId":"customer-schema","workerId":WorkerId::mint(),"revision":1,"expiresAt":1000});
    assert!(serde_json::from_value::<Assignment>(assignment).is_err());
}
