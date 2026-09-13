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
