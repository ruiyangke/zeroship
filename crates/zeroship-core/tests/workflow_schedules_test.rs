use serde::{de::DeserializeOwned, Serialize};
use serde_json::{json, Value};
use std::fmt::Debug;
use zeroship_core::{
    app_id::AppId,
    workflow_coordination::RunId,
    workflow_jobs::{DeploymentId, JobId},
    workflow_schedules::{
        ActivateSchedules, RegisterSchedules, ScheduleCatchUp, ScheduleDescriptor, ScheduleId,
        ScheduleOverlap, ScheduleTiming,
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
        "unexpectedly accepted scheduling metadata: {wire}"
    );
}

fn descriptors() -> Vec<(ScheduleDescriptor, Value)> {
    let mut cases = Vec::new();
    for (schedule, wire) in [
        (
            ScheduleTiming::Cron {
                cron_expr: "0 9 * * *".into(),
                tz: "America/Los_Angeles".into(),
            },
            json!({"kind":"cron","cron_expr":"0 9 * * *","tz":"America/Los_Angeles"}),
        ),
        (
            ScheduleTiming::Interval {
                interval_ms: 60_000,
                anchor: serde_json::from_value(json!("deploy")).unwrap(),
            },
            json!({"kind":"interval","interval_ms":60_000,"anchor":"deploy"}),
        ),
    ] {
        for (overlap, catch_up, overlap_wire, catch_up_wire) in [
            (
                ScheduleOverlap::Allow,
                ScheduleCatchUp::Skip,
                json!("allow"),
                json!({"mode":"skip"}),
            ),
            (
                ScheduleOverlap::SkipIfRunning,
                ScheduleCatchUp::Backfill { max: 3 },
                json!("skipIfRunning"),
                json!({"mode":"backfill","max":3}),
            ),
        ] {
            cases.push((
                ScheduleDescriptor {
                    name: "daily-report".into(),
                    workflow_name: "Report".into(),
                    schedule: schedule.clone(),
                    overlap,
                    catch_up,
                },
                json!({
                    "name":"daily-report","workflowName":"Report","schedule":wire,
                    "overlap":overlap_wire,"catchUp":catch_up_wire,
                }),
            ));
        }
    }
    cases
}

fn registration(schedules: Vec<ScheduleDescriptor>) -> RegisterSchedules {
    RegisterSchedules {
        app_id: AppId::mint(),
        deployment_id: DeploymentId::mint(),
        schedules,
    }
}

#[test]
fn schedule_registration_and_activation_preserve_explicit_metadata_identity() {
    let cases = descriptors();
    assert!(!cases.is_empty());
    for (descriptor, expected) in &cases {
        assert_eq!(round_trip(descriptor), *expected);
    }
    let expected: Vec<_> = cases.iter().map(|(_, wire)| wire.clone()).collect();
    let request = registration(
        cases
            .into_iter()
            .map(|(descriptor, _)| descriptor)
            .collect(),
    );
    assert_eq!(
        round_trip(&request),
        json!({
            "appId":request.app_id,"deploymentId":request.deployment_id,
            "schedules":expected,
        })
    );
    let activation = ActivateSchedules {
        app_id: request.app_id.clone(),
        deployment_id: request.deployment_id.clone(),
        revision: 7.try_into().unwrap(),
    };
    assert_eq!(
        round_trip(&activation),
        json!({"appId":request.app_id,"deploymentId":request.deployment_id,"revision":7})
    );
    round_trip(&RegisterSchedules {
        schedules: Vec::new(),
        ..request
    });
    let schedule = ScheduleId::mint();
    assert_eq!(ScheduleId::parse(schedule.as_str()).unwrap(), schedule);
    assert_eq!(round_trip(&schedule), json!(schedule.as_str()));
}

#[test]
fn schedule_descriptors_default_only_overlap_and_catch_up() {
    for (descriptor, mut wire) in descriptors() {
        wire.as_object_mut().unwrap().remove("overlap");
        wire.as_object_mut().unwrap().remove("catchUp");
        assert_eq!(
            serde_json::from_value::<ScheduleDescriptor>(wire.clone()).unwrap(),
            ScheduleDescriptor {
                overlap: ScheduleOverlap::Allow,
                catch_up: ScheduleCatchUp::Skip,
                ..descriptor
            }
        );
        for field in ["name", "workflowName", "schedule"] {
            let mut missing = wire.clone();
            missing.as_object_mut().unwrap().remove(field);
            refuses::<ScheduleDescriptor>(missing);
        }
        for field in ["overlap", "catchUp"] {
            let mut invalid = wire.clone();
            invalid[field] = Value::Null;
            refuses::<ScheduleDescriptor>(invalid);
        }
    }
}

#[test]
fn scheduling_messages_reject_customer_data_at_every_nested_boundary() {
    for (descriptor, _) in descriptors() {
        let request = registration(vec![descriptor]);
        let wire = round_trip(&request);
        for path in [
            "",
            "/schedules/0",
            "/schedules/0/schedule",
            "/schedules/0/catchUp",
        ] {
            for field in [
                "input",
                "body",
                "history",
                "result",
                "credentials",
                "databaseUrl",
                "unknown",
            ] {
                let mut invalid = wire.clone();
                invalid
                    .pointer_mut(path)
                    .unwrap()
                    .as_object_mut()
                    .unwrap()
                    .insert(field.into(), json!({"private":"customer-data"}));
                refuses::<RegisterSchedules>(invalid);
            }
        }
    }
    let activation = ActivateSchedules {
        app_id: AppId::mint(),
        deployment_id: DeploymentId::mint(),
        revision: 1.try_into().unwrap(),
    };
    let wire = round_trip(&activation);
    for field in [
        "schedules",
        "input",
        "body",
        "credentials",
        "databaseUrl",
        "unknown",
    ] {
        let mut invalid = wire.clone();
        invalid[field] = json!({"private":"customer-data"});
        refuses::<ActivateSchedules>(invalid);
    }
}

#[test]
fn scheduling_envelopes_require_native_identities_and_activation_revisions() {
    let wire = round_trip(&registration(Vec::new()));
    for field in ["appId", "deploymentId", "schedules"] {
        let mut missing = wire.clone();
        missing.as_object_mut().unwrap().remove(field);
        refuses::<RegisterSchedules>(missing);
    }
    for (field, foreign) in [
        ("appId", json!(DeploymentId::mint())),
        ("deploymentId", json!(AppId::mint())),
    ] {
        let mut invalid = wire.clone();
        invalid[field] = foreign;
        refuses::<RegisterSchedules>(invalid);
    }
    for bad in [Value::Null, json!({"Report":{}}), json!("Report")] {
        let mut invalid = wire.clone();
        invalid["schedules"] = bad;
        refuses::<RegisterSchedules>(invalid);
    }
    for bad in [
        json!(""),
        json!("wsc_"),
        json!(JobId::mint()),
        json!(RunId::mint()),
        Value::Null,
        json!(1),
    ] {
        refuses::<ScheduleId>(bad);
    }
    let activation = ActivateSchedules {
        app_id: AppId::mint(),
        deployment_id: DeploymentId::mint(),
        revision: 1.try_into().unwrap(),
    };
    let wire = round_trip(&activation);
    for field in ["appId", "deploymentId", "revision"] {
        let mut missing = wire.clone();
        missing.as_object_mut().unwrap().remove(field);
        refuses::<ActivateSchedules>(missing);
    }
    for (field, foreign) in [
        ("appId", json!(DeploymentId::mint())),
        ("deploymentId", json!(AppId::mint())),
    ] {
        let mut invalid = wire.clone();
        invalid[field] = foreign;
        refuses::<ActivateSchedules>(invalid);
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
        invalid["revision"] = bad;
        refuses::<ActivateSchedules>(invalid);
    }
    let mut maximum = wire;
    maximum["revision"] = json!(i64::MAX);
    round_trip(&serde_json::from_value::<ActivateSchedules>(maximum).unwrap());
}

#[test]
fn nested_schedule_policies_are_closed_and_require_their_selected_fields() {
    for (descriptor, _) in descriptors() {
        let wire = round_trip(&descriptor);
        for path in ["/schedule", "/catchUp"] {
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
                refuses::<ScheduleDescriptor>(missing);
            }
        }
        for field in ["name", "workflowName", "overlap"] {
            let mut invalid = wire.clone();
            invalid[field] = json!({"input":"private"});
            refuses::<ScheduleDescriptor>(invalid);
        }
        for (field, invalid_value) in [
            ("overlap", json!("skip")),
            ("catchUp", json!({"mode":"unknown"})),
            ("catchUp", json!({"mode":"skip","max":3})),
            ("catchUp", json!({"mode":"backfill","max":-1})),
            ("catchUp", json!({"mode":"backfill","max":"1"})),
            (
                "schedule",
                json!({"kind":"interval","interval_ms":"1000","anchor":"epoch"}),
            ),
            (
                "schedule",
                json!({"kind":"interval","interval_ms":1000,"anchor":"now"}),
            ),
            ("schedule", json!({"kind":"cron","cron_expr":{},"tz":"UTC"})),
        ] {
            let mut invalid = wire.clone();
            invalid[field] = invalid_value;
            refuses::<ScheduleDescriptor>(invalid);
        }
    }
}
