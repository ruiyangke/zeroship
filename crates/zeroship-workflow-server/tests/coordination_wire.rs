use serde::{de::DeserializeOwned, Serialize};
use serde_json::{json, Value};
use std::fmt::Debug;
use zeroship_core::{
    app_id::AppId,
    workflow_coordination::{
        AcknowledgeManagement, AssignScope, AssignedScope, Assignment, ManageRun,
        ManagementOperation, ManagementOutcome, ManagementReceipt, PublishWakeHint, RegisterWorker,
        RegisteredWorker, ReleaseScope, RequestId, Revision, RunId, UnixMillis, WakeHintReceipt,
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
    rejects_customer_fields::<RegisterWorker>(&json!({"capacity":4,"state":"ready"}));
    rejects_customer_fields::<RegisteredWorker>(&json!({
        "workerId":worker,"capacity":4,"state":"ready","expiresAt":1000
    }));
    rejects_customer_fields::<AssignScope>(&json!({
        "requestId":request,"appId":app,"workerId":worker,"expectedRevision":null
    }));
    rejects_customer_fields::<Assignment>(&json!({
        "appId":app,"workerId":worker,"revision":1,"expiresAt":1000
    }));
    rejects_customer_fields::<AssignedScope>(&json!({
        "appId":app,"assignmentRevision":1
    }));
    rejects_customer_fields::<ReleaseScope>(&json!({
        "requestId":request,"appId":app,"assignmentRevision":1,"wakeRevision":2
    }));
    assert!(
        serde_json::from_value::<ReleaseScope>(json!({
            "requestId":request,"appId":app,"assignmentRevision":1
        }))
        .is_err(),
        "release must identify the acknowledged wake hint"
    );
    rejects_customer_fields::<PublishWakeHint>(&json!({
        "appId":app,"assignmentRevision":1,"revision":2,"nextDueAt":1000
    }));
    rejects_customer_fields::<WakeHintReceipt>(&json!({
        "appId":app,"assignmentRevision":1,"revision":2
    }));
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
    for command in [
        json!({"kind":"transition","operation":"pause"}),
        json!({"kind":"restart","options":{"from":{"name":"checkpoint","occurrence":0},"deploy":"started"}}),
    ] {
        rejects_customer_fields::<ManagementOperation>(&command);
        let envelope =
            json!({"requestId":request,"appId":app,"runId":RunId::mint(),"command":command});
        rejects_customer_fields::<ManageRun>(&envelope);
        let mut attempted = envelope;
        attempted["command"]["input"] = json!("private");
        assert!(serde_json::from_value::<ManageRun>(attempted).is_err());
    }
    for outcome in [
        json!({"kind":"applied","state":"paused"}),
        json!({"kind":"not_found"}),
        json!({"kind":"conflict"}),
        json!({"kind":"denied"}),
    ] {
        rejects_customer_fields::<ManagementOutcome>(&outcome);
        rejects_customer_fields::<AcknowledgeManagement>(&json!({
            "requestId":request,"appId":app,"assignmentRevision":1,"outcome":outcome
        }));
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
