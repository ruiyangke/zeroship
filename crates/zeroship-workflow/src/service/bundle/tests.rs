use super::*;
use serde_json::{json, Value};
use zeroship_bundle::{sha256_hex, LocalDiskBlobStore, WorkerCode};

fn interval(name: &str) -> Value {
    json!({
        "name": name,
        "workflowName": "Alpha",
        "schedule": {"kind":"interval", "interval_ms":1000, "anchor":"deploy"},
    })
}

fn manifest(schedules: Vec<Value>) -> Manifest {
    Manifest {
        workflows: Some(json!(["Zulu", "Alpha"])),
        schedules,
        ..Manifest::default()
    }
}

fn invalid(manifest: &Manifest) {
    assert!(matches!(
        BundleDeclarations::parse(manifest),
        Err(ExecutableError::InvalidManifest)
    ));
}

#[test]
fn projection_omits_input_and_preserves_canonical_creator_registration() {
    let input = json!({
        "nested": [{"customerMarker":"creator-private-input", "count":17}],
        "schedule": {"workflowName":"business-data", "input":[null, true]},
    });
    let mut alpha = interval("a-calendar");
    alpha["input"] = input.clone();
    let zulu = json!({
        "name":"z-calendar", "workflowName":"Zulu",
        "schedule":{"kind":"cron", "cron_expr":"0 0 * * *", "tz":"UTC"},
        "input":["retained-business-value"],
        "overlap":"skipIfRunning", "catchUp":{"mode":"backfill", "max":2},
    });
    let original = manifest(vec![zulu.clone(), alpha.clone()]);
    let declarations = BundleDeclarations::parse(&original).unwrap();
    assert_eq!(
        declarations.workflows(),
        &BTreeSet::from(["Alpha".into(), "Zulu".into()])
    );
    let registration = declarations.registration("chosen-deployment".into(), "a".repeat(64));
    assert_eq!(registration.id, "chosen-deployment");
    assert_eq!(registration.hash, "a".repeat(64));
    assert_eq!(registration.schedules[0].input, input);
    alpha["overlap"] = json!("allow");
    alpha["catchUp"] = json!({"mode":"skip"});
    assert_eq!(
        serde_json::to_value(&registration.schedules).unwrap(),
        json!([alpha, zulu])
    );
    assert_eq!(
        serde_json::to_value(declarations.manager_schedules()).unwrap(),
        json!([
            {
                "name":"a-calendar", "workflowName":"Alpha",
                "schedule":{"kind":"interval", "interval_ms":1000, "anchor":"deploy"},
                "overlap":"allow", "catchUp":{"mode":"skip"},
            },
            {
                "name":"z-calendar", "workflowName":"Zulu",
                "schedule":{"kind":"cron", "cron_expr":"0 0 * * *", "tz":"UTC"},
                "overlap":"skipIfRunning", "catchUp":{"mode":"backfill", "max":2},
            },
        ])
    );
    assert_eq!(
        registration,
        declarations.registration("chosen-deployment".into(), "a".repeat(64))
    );
    let mut reordered = original;
    reordered.schedules.reverse();
    reordered.workflows = Some(json!(["Alpha", "Zulu"]));
    assert_eq!(BundleDeclarations::parse(&reordered).unwrap(), declarations);
}

#[test]
fn empty_declarations_and_creator_defaults_are_explicit() {
    for workflows in [None, Some(json!([]))] {
        let declarations = BundleDeclarations::parse(&Manifest {
            workflows,
            ..Manifest::default()
        })
        .unwrap();
        assert!(declarations.workflows().is_empty());
        assert!(declarations.manager_schedules().is_empty());
        assert!(declarations
            .registration("id".into(), "hash".into())
            .schedules
            .is_empty());
    }
    let declarations = BundleDeclarations::parse(&manifest(vec![interval("defaulted")])).unwrap();
    let registration = declarations.registration("id".into(), "hash".into());
    assert_eq!(registration.schedules[0].input, Value::Null);
    assert_eq!(
        registration.schedules[0].overlap,
        super::super::ScheduleOverlap::Allow
    );
    assert_eq!(registration.schedules[0].catch_up, ScheduleCatchUp::Skip);
    assert_eq!(
        serde_json::to_value(&registration.schedules[0]).unwrap(),
        json!({
            "name":"defaulted", "workflowName":"Alpha",
            "schedule":{"kind":"interval", "interval_ms":1000, "anchor":"deploy"},
            "input":null, "overlap":"allow", "catchUp":{"mode":"skip"},
        })
    );
}

#[test]
fn workflow_declarations_are_an_exact_unique_name_list() {
    for workflows in [
        json!(null),
        json!({"Alpha":true}),
        json!("Alpha"),
        json!([17]),
        json!(["Alpha", "Alpha"]),
        json!([""]),
        json!(["__zs.internal"]),
        json!(["w".repeat(validation::WORKFLOW_NAME_MAX_BYTES + 1)]),
    ] {
        invalid(&Manifest {
            workflows: Some(workflows),
            ..Manifest::default()
        });
    }
    BundleDeclarations::parse(&manifest(vec![])).unwrap();
}

#[test]
fn schedule_declarations_reject_aliases_and_unknown_metadata() {
    let baseline = interval("strict");
    BundleDeclarations::parse(&manifest(vec![baseline.clone()])).unwrap();
    for (key, value) in [
        ("workflow_name", json!("Alpha")),
        ("catch_up", json!({"mode":"skip"})),
        ("unknown", json!(true)),
        ("workerUrl", json!("customer-selected-host")),
    ] {
        let mut schedule = baseline.clone();
        schedule[key] = value;
        invalid(&manifest(vec![schedule]));
    }
    for timing in [
        json!({"kind":"interval", "intervalMs":1000, "anchor":"deploy"}),
        json!({"kind":"interval", "interval_ms":1000, "anchor":"deploy", "unknown":true}),
        json!({"kind":"cron", "cronExpr":"@daily", "tz":"UTC"}),
        json!({"kind":"cron", "cron_expr":"@daily", "tz":"UTC", "unknown":true}),
    ] {
        let mut schedule = baseline.clone();
        schedule["schedule"] = timing;
        invalid(&manifest(vec![schedule]));
    }
    for catch_up in [
        json!({"mode":"skip", "max":1}),
        json!({"mode":"skip", "unknown":true}),
        json!({"mode":"backfill", "max":1, "unknown":true}),
    ] {
        let mut schedule = baseline.clone();
        schedule["catchUp"] = catch_up;
        invalid(&manifest(vec![schedule]));
    }
    let mut missing = baseline;
    missing.as_object_mut().unwrap().remove("workflowName");
    invalid(&manifest(vec![missing]));
}

#[test]
fn schedule_names_exports_and_calendar_shapes_are_validated() {
    let baseline = interval("checked");
    invalid(&manifest(vec![baseline.clone(), baseline.clone()]));
    for name in [
        "".to_owned(),
        "__zs.internal".into(),
        "s".repeat(validation::WORKFLOW_NAME_MAX_BYTES + 1),
    ] {
        let mut schedule = baseline.clone();
        schedule["name"] = json!(name);
        invalid(&manifest(vec![schedule]));
    }
    let mut missing_export = baseline.clone();
    missing_export["workflowName"] = json!("Missing");
    invalid(&manifest(vec![missing_export]));
    invalid(&Manifest {
        schedules: vec![baseline.clone()],
        ..Manifest::default()
    });
    for timing in [
        json!({"kind":"interval", "interval_ms":0, "anchor":"epoch"}),
        json!({"kind":"interval", "interval_ms":-1, "anchor":"deploy"}),
        json!({"kind":"cron", "cron_expr":"not a calendar", "tz":"UTC"}),
        json!({"kind":"cron", "cron_expr":"@daily", "tz":"not/a-timezone"}),
    ] {
        let mut schedule = baseline.clone();
        schedule["schedule"] = timing;
        invalid(&manifest(vec![schedule]));
    }
    for max in [json!(0), json!(-1), json!(1.5), Value::Null] {
        let mut schedule = baseline.clone();
        schedule["catchUp"] = json!({"mode":"backfill", "max":max});
        invalid(&manifest(vec![schedule]));
    }
    let mut host_limited = baseline;
    host_limited["schedule"]["interval_ms"] = json!(1);
    host_limited["catchUp"] = json!({"mode":"backfill", "max":usize::MAX});
    BundleDeclarations::parse(&manifest(vec![host_limited])).unwrap();
}

#[compio::test]
async fn executable_reuses_declarations_and_charges_full_creator_metadata() {
    let work = tempfile::tempdir().unwrap();
    let store = LocalDiskBlobStore::new(work.path().into()).unwrap();
    let source = b"export default 'normal app';";
    let hash = sha256_hex(source);
    let mut schedule = interval("budgeted");
    schedule["input"] = json!({"nested":[{"customerMarker":"retained input"}]});
    let mut manifest = manifest(vec![schedule]);
    manifest.worker = Some(WorkerCode {
        entry: "entry.js".into(),
        modules: [("entry.js".into(), hash.clone())].into(),
    });
    // Declaration projection succeeds even before the referenced blob exists.
    let declarations = BundleDeclarations::parse(&manifest).unwrap();
    assert!(!declarations.manager_schedules().is_empty());
    assert!(matches!(
        BundleExecutable::load(&manifest, &store, 4096).await,
        Err(ExecutableError::Storage(_))
    ));
    store.put_blob(&hash, source).await.unwrap();
    let registration = declarations.registration("deployment".into(), "b".repeat(64));
    let declaration_bytes = serde_json::to_vec(&(&registration.workflows, &registration.schedules))
        .unwrap()
        .len();
    let executable_bytes = serde_json::to_vec(manifest.worker.as_ref().unwrap())
        .unwrap()
        .len()
        + source.len();
    let total = declaration_bytes + executable_bytes;
    LoadedWorker::load(&manifest, &store, executable_bytes)
        .await
        .unwrap();
    assert!(matches!(
        BundleExecutable::load(&manifest, &store, executable_bytes).await,
        Err(ExecutableError::TooLarge)
    ));
    assert!(matches!(
        BundleExecutable::load(&manifest, &store, total - 1).await,
        Err(ExecutableError::TooLarge)
    ));
    let loaded = BundleExecutable::load(&manifest, &store, total)
        .await
        .unwrap();
    assert_eq!(
        loaded.registration("deployment".into(), "b".repeat(64)),
        registration
    );
    assert_eq!(loaded.executable().modules()["entry.js"].as_bytes(), source);
    manifest.workflows = Some(json!(["Alpha", "Alpha"]));
    assert!(matches!(
        BundleExecutable::load(&manifest, &store, total).await,
        Err(ExecutableError::InvalidManifest)
    ));
}
