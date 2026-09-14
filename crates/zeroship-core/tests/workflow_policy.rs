use serde_json::{json, Value};
use zeroship_core::workflow_policy::{
    AppPolicy, InvalidPolicy, SIGNAL_CAPABILITY_MAX_LIFETIME_SECONDS,
};

fn document() -> Value {
    serde_json::to_value(AppPolicy::default()).unwrap()
}

fn decode_with(field: &str, value: Value) -> Result<AppPolicy, serde_json::Error> {
    let mut document = document();
    assert!(
        document.get(field).is_some(),
        "unknown fixture field: {field}"
    );
    document[field] = value;
    serde_json::from_value(document)
}

#[test]
fn raw_policy_requires_complete_closed_values() {
    let policy = AppPolicy::default();
    policy.validate().unwrap();
    let document = document();
    assert_eq!(
        serde_json::from_value::<AppPolicy>(document.clone()).unwrap(),
        policy
    );
    let fields = document.as_object().unwrap();
    assert!(!fields.is_empty());
    for field in fields.keys() {
        let mut incomplete = fields.clone();
        incomplete.remove(field);
        assert!(
            serde_json::from_value::<AppPolicy>(Value::Object(incomplete)).is_err(),
            "missing policy field accepted: {field}"
        );
    }
    let mut extra = document.clone();
    extra["databaseUrl"] = json!("untrusted");
    assert!(serde_json::from_value::<AppPolicy>(extra).is_err());
    let mut wrong_case = document;
    let value = wrong_case
        .as_object_mut()
        .unwrap()
        .remove("maxLiveRuns")
        .unwrap();
    wrong_case["max_live_runs"] = value;
    assert!(serde_json::from_value::<AppPolicy>(wrong_case).is_err());
}

#[test]
fn disabling_limits_and_independent_switches_are_valid() {
    for admission in [false, true] {
        for dispatch in [false, true] {
            for ingress in [false, true] {
                AppPolicy {
                    admission,
                    dispatch,
                    ingress,
                    max_live_runs: 0,
                    max_child_depth: 0,
                    max_running: 0,
                    max_schedules: 0,
                    ..AppPolicy::default()
                }
                .validate()
                .unwrap();
            }
        }
    }
}

#[test]
fn shared_validation_rejects_invalid_resource_limits() {
    let invalid = [
        ("maxLiveRuns", json!(-1)),
        ("maxChildDepth", json!(-1)),
        ("maxRunning", json!(-1)),
        ("maxInputBytes", json!(0)),
        ("maxFrontier", json!(0)),
        ("maxJournalBytes", json!(0)),
        ("maxPayloadBytes", json!(0)),
        ("maxPayloadObjects", json!(0)),
        ("maxPayloadStorageBytes", json!(0)),
        ("payloadStagingRetentionMs", json!(0)),
        ("maxCompensationAttempts", json!(0)),
        ("compensationRetryMs", json!(0)),
        ("maxScheduleBackfill", json!(0)),
        ("minScheduleIntervalMs", json!(0)),
        ("maxSignalTokenLifetimeSeconds", json!(0)),
        ("leaseMs", json!(0)),
    ];
    for (field, value) in invalid {
        // Decoding raw values grants no authority: every host must also use the
        // shared validator when constructing its trusted policy grant.
        let policy = decode_with(field, value).unwrap();
        assert_eq!(policy.validate(), Err(InvalidPolicy), "{field}");
    }
}

#[test]
fn relational_limits_and_capability_ceiling_keep_their_boundaries() {
    let mut policy = AppPolicy::default();
    policy.max_payload_storage_bytes = policy.max_payload_bytes;
    policy.max_signal_token_lifetime_seconds = SIGNAL_CAPABILITY_MAX_LIFETIME_SECONDS;
    policy.validate().unwrap();
    policy.max_payload_storage_bytes -= 1;
    assert_eq!(policy.validate(), Err(InvalidPolicy));
    policy.max_payload_storage_bytes = policy.max_payload_bytes;
    policy.max_signal_token_lifetime_seconds += 1;
    assert_eq!(policy.validate(), Err(InvalidPolicy));
}

#[test]
fn unsigned_limits_reject_unrepresentable_values_without_clamping() {
    let fields = [
        "maxInputBytes",
        "maxFrontier",
        "maxJournalBytes",
        "maxSchedules",
        "maxScheduleBackfill",
    ];
    for field in fields {
        let boundary = decode_with(field, json!(usize::MAX)).unwrap();
        boundary.validate().unwrap();
        assert_eq!(
            serde_json::to_value(boundary).unwrap()[field],
            json!(usize::MAX)
        );
        for invalid in [json!(-1), json!(1.5), json!("1"), Value::Null] {
            assert!(decode_with(field, invalid).is_err(), "{field}");
        }
        let mut too_large = document();
        too_large[field] = json!("unrepresentable-integer");
        let wire = serde_json::to_string(&too_large).unwrap().replace(
            "\"unrepresentable-integer\"",
            &(usize::MAX as u128 + 1).to_string(),
        );
        assert!(serde_json::from_str::<AppPolicy>(&wire).is_err(), "{field}");
    }
}

#[test]
fn fixed_integer_fields_reject_wire_overflow_and_coercion() {
    assert!(decode_with("maxCompensationAttempts", json!(i64::from(i32::MAX) + 1)).is_err());
    assert!(decode_with("leaseMs", json!(u64::MAX)).is_err());
    for invalid in [json!(0), json!("false"), Value::Null] {
        assert!(decode_with("admission", invalid).is_err());
    }
}

#[test]
fn lease_requests_are_closed_and_name_an_optional_prior_epoch() {
    use zeroship_core::{
        app_id::AppId, workflow_coordination::AssignedScope, workflow_policy::PolicyLeaseRequest,
    };
    let scope = AssignedScope {
        app_id: AppId::mint(),
        assignment_revision: 2.try_into().unwrap(),
    };
    let plain = PolicyLeaseRequest {
        scope: scope.clone(),
        establish_after: None,
        ingress_used: false,
    };
    let wire = serde_json::to_value(&plain).unwrap();
    assert_eq!(
        wire,
        json!({"scope":{"appId":scope.app_id,"assignmentRevision":2},
            "establishAfter":null,"ingressUsed":false})
    );
    assert_eq!(
        serde_json::from_value::<PolicyLeaseRequest>(wire.clone()).unwrap(),
        plain
    );
    let establish = PolicyLeaseRequest {
        establish_after: Some(5.try_into().unwrap()),
        ingress_used: true,
        ..plain
    };
    let established = serde_json::to_value(&establish).unwrap();
    assert_eq!(established["establishAfter"], json!(5));
    assert_eq!(
        serde_json::from_value::<PolicyLeaseRequest>(established.clone()).unwrap(),
        establish
    );
    for bad in [json!(0), json!(-1), json!(1.5), json!("1")] {
        let mut invalid = established.clone();
        invalid["establishAfter"] = bad;
        assert!(serde_json::from_value::<PolicyLeaseRequest>(invalid).is_err());
    }
    for bad in [Value::Null, json!(0), json!("true")] {
        let mut invalid = established.clone();
        invalid["ingressUsed"] = bad;
        assert!(serde_json::from_value::<PolicyLeaseRequest>(invalid).is_err());
    }
    let mut missing = established.clone();
    missing.as_object_mut().unwrap().remove("ingressUsed");
    assert!(serde_json::from_value::<PolicyLeaseRequest>(missing).is_err());
    for (path, field) in [("", "epoch"), ("", "expiresAt"), ("/scope", "workerId")] {
        let mut invalid = established.clone();
        invalid
            .pointer_mut(path)
            .unwrap()
            .as_object_mut()
            .unwrap()
            .insert(field.into(), json!(1));
        assert!(serde_json::from_value::<PolicyLeaseRequest>(invalid).is_err());
    }
}

#[test]
fn leases_carry_an_optional_positive_ingress_epoch() {
    use zeroship_core::{
        app_id::AppId, workflow_coordination::WorkerId, workflow_policy::PolicyLease,
    };
    let lease = PolicyLease {
        app_id: AppId::mint(),
        worker_id: WorkerId::mint(),
        signing_key_id: "thumbprint".into(),
        assignment_revision: 1.try_into().unwrap(),
        policy_revision: 1.try_into().unwrap(),
        policy: AppPolicy::default(),
        ingress_epoch: Some(3.try_into().unwrap()),
        remaining_ms: std::num::NonZeroU64::new(10).unwrap(),
    };
    let wire = serde_json::to_value(&lease).unwrap();
    assert_eq!(wire["ingressEpoch"], json!(3));
    assert_eq!(serde_json::from_value::<PolicyLease>(wire.clone()).unwrap(), lease);
    let retired = PolicyLease {
        ingress_epoch: None,
        ..lease
    };
    let retired_wire = serde_json::to_value(&retired).unwrap();
    assert_eq!(retired_wire["ingressEpoch"], Value::Null);
    assert_eq!(
        serde_json::from_value::<PolicyLease>(retired_wire).unwrap(),
        retired
    );
    for bad in [json!(0), json!(-1), json!("3"), json!(1.5)] {
        let mut invalid = wire.clone();
        invalid["ingressEpoch"] = bad;
        assert!(serde_json::from_value::<PolicyLease>(invalid).is_err());
    }
    let mut extra = wire;
    extra["closedEpoch"] = json!(2);
    assert!(serde_json::from_value::<PolicyLease>(extra).is_err());
}
