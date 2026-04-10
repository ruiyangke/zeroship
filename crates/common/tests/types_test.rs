use appbase_common::types::{AppUsage, ControlEvent, RouteEntry, UsageReport};
use std::collections::HashMap;
use uuid::Uuid;

#[test]
fn control_event_deploy_json() {
    let app_id = Uuid::new_v4();
    let event = ControlEvent::Deploy {
        app_id,
        hash: "abc123".to_string(),
    };

    let json = serde_json::to_string(&event).unwrap();
    // The internally tagged enum uses the variant name as the "type" value.
    assert!(
        json.contains(r#""type":"Deploy""#),
        "expected \"type\":\"Deploy\" in JSON, got: {}",
        json
    );
    assert!(json.contains(&app_id.to_string()));
    assert!(json.contains("abc123"));

    // Round-trip.
    let decoded: ControlEvent = serde_json::from_str(&json).unwrap();
    match decoded {
        ControlEvent::Deploy {
            app_id: id,
            hash: h,
        } => {
            assert_eq!(id, app_id);
            assert_eq!(h, "abc123");
        }
        other => panic!("expected Deploy, got {:?}", other),
    }
}

#[test]
fn control_event_delete_json() {
    let app_id = Uuid::new_v4();
    let event = ControlEvent::Delete { app_id };

    let json = serde_json::to_string(&event).unwrap();
    assert!(
        json.contains(r#""type":"Delete""#),
        "expected \"type\":\"Delete\" in JSON, got: {}",
        json
    );
    assert!(json.contains(&app_id.to_string()));

    // Round-trip.
    let decoded: ControlEvent = serde_json::from_str(&json).unwrap();
    match decoded {
        ControlEvent::Delete { app_id: id } => {
            assert_eq!(id, app_id);
        }
        other => panic!("expected Delete, got {:?}", other),
    }
}

#[test]
fn usage_report_roundtrip() {
    let worker_id = Uuid::new_v4();
    let app_id = Uuid::new_v4();

    let mut counters: HashMap<Uuid, AppUsage> = HashMap::new();
    counters.insert(
        app_id,
        AppUsage {
            requests: 42,
            cpu_us: 1000,
            wall_us: 2000,
            egress_bytes: 512,
            ingress_bytes: 256,
        },
    );

    let report = UsageReport { worker_id, counters };
    let json = serde_json::to_string(&report).unwrap();
    let decoded: UsageReport = serde_json::from_str(&json).unwrap();

    assert_eq!(decoded.worker_id, worker_id);
    let usage = decoded.counters.get(&app_id).unwrap();
    assert_eq!(usage.requests, 42);
    assert_eq!(usage.cpu_us, 1000);
    assert_eq!(usage.wall_us, 2000);
    assert_eq!(usage.egress_bytes, 512);
    assert_eq!(usage.ingress_bytes, 256);
}

#[test]
fn route_entry_roundtrip() {
    let plan_id = Uuid::new_v4();
    let entry = RouteEntry {
        name: "my-app".to_string(),
        plan_id,
        api_key_hash: "deadbeef".repeat(8),
        deploy_hash: Some("v1.2.3".to_string()),
    };

    let json = serde_json::to_string(&entry).unwrap();
    let decoded: RouteEntry = serde_json::from_str(&json).unwrap();

    assert_eq!(decoded.name, "my-app");
    assert_eq!(decoded.plan_id, plan_id);
    assert_eq!(decoded.api_key_hash, "deadbeef".repeat(8));
    assert_eq!(decoded.deploy_hash, Some("v1.2.3".to_string()));
}
