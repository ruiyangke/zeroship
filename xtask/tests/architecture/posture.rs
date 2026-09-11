use super::{repo, source};
use regex::Regex;
use std::collections::BTreeSet;

fn mandatory_database_tests(package: &serde_json::Value) -> Result<usize, String> {
    let name = package["name"].as_str().ok_or("package name missing")?;
    if package["features"].get("live-db-tests").is_some() {
        return Err(format!(
            "{name}: live database verification cannot be optional"
        ));
    }
    let targets = package["targets"].as_array().ok_or("targets missing")?;
    let mut examined = 0;
    for target in targets.iter().filter(|target| target["test"] == true) {
        examined += 1;
        if target["required-features"]
            .as_array()
            .is_some_and(|features| !features.is_empty())
        {
            return Err(format!(
                "{name}: {} hides tests behind a feature",
                target["name"]
            ));
        }
    }
    if examined == 0 {
        return Err(format!("{name}: no ordinary test targets"));
    }
    Ok(examined)
}

#[test]
fn platform_database_tests_are_mandatory() {
    let workspace = repo::workspace();
    assert!(
        workspace.len() >= 10,
        "workspace feature scan lost its corpus"
    );
    for package in workspace {
        assert!(
            package["features"].get("live-db-tests").is_none(),
            "{} declares optional live database verification",
            package["name"]
        );
    }
    for name in [
        "zeroship-control",
        "zeroship-migrate-server",
        "zeroship-worker",
    ] {
        let examined = mandatory_database_tests(repo::package(name)).unwrap();
        assert!(
            examined >= 2,
            "{name}: database test target scan lost its corpus"
        );
    }
}

#[test]
fn mandatory_database_test_check_rejects_feature_and_target_gates() {
    let ordinary = serde_json::json!({
        "name": "service", "features": {},
        "targets": [{"name": "database", "test": true}]
    });
    assert_eq!(mandatory_database_tests(&ordinary).unwrap(), 1);
    let mut hidden = ordinary.clone();
    hidden["features"]["live-db-tests"] = serde_json::json!([]);
    assert!(mandatory_database_tests(&hidden).is_err());
    let mut hidden = ordinary.clone();
    hidden["targets"][0]["required-features"] = serde_json::json!(["optional-db"]);
    assert!(mandatory_database_tests(&hidden).is_err());
    let mut hidden = ordinary;
    hidden["targets"][0]["test"] = serde_json::json!(false);
    assert!(mandatory_database_tests(&hidden).is_err());
}

fn finite_wal_limit(document: &str) -> Result<(), String> {
    let parsed: serde_yaml::Value = serde_yaml::from_str(document).map_err(|e| e.to_string())?;
    let command = parsed["services"]["postgres"]["command"]
        .as_sequence()
        .ok_or("Postgres command must be an argument array")?;
    let command: Vec<_> = command
        .iter()
        .map(|v| v.as_str().ok_or("Command arguments must be strings"))
        .collect::<Result<_, _>>()?;
    let assignments: Vec<_> = command
        .iter()
        .enumerate()
        .filter(|(_, s)| s.starts_with("max_slot_wal_keep_size="))
        .collect();
    if assignments.len() != 1 {
        return Err("Postgres must declare its WAL limit exactly once".into());
    }
    let (index, assignment) = assignments[0];
    if index == 0 || command[index - 1] != "-c" {
        return Err("WAL limit must follow -c".into());
    }
    if !Regex::new(r"^(0|[1-9][0-9]*)(kB|MB|GB|TB)?$")
        .unwrap()
        .is_match(assignment.split_once('=').unwrap().1)
    {
        return Err("WAL limit must be finite and nonnegative".into());
    }
    Ok(())
}

#[test]
fn compose_declares_a_finite_postgres_wal_limit() {
    finite_wal_limit(&repo::read("deploy/compose/docker-compose.yml")).unwrap();
}

#[test]
fn wal_limit_validation_refuses_missing_duplicate_unlimited_and_unbound_values() {
    for command in [
        "[]",
        "['postgres']",
        "['-c', 'max_slot_wal_keep_size=-1']",
        "['max_slot_wal_keep_size=1GB']",
        "['-c', 'max_slot_wal_keep_size=1GB', '-c', 'max_slot_wal_keep_size=2GB']",
        "['-c', 'max_slot_wal_keep_size=forever']",
    ] {
        assert!(
            finite_wal_limit(&format!("services:\n  postgres:\n    command: {command}\n")).is_err(),
            "{command}"
        );
    }
    for value in ["0", "1", "100kB", "100MB", "1GB", "1TB"] {
        finite_wal_limit(&format!("services:\n  postgres:\n    command: ['postgres', '-c', 'max_slot_wal_keep_size={value}']\n")).unwrap();
    }
}

fn replication_use(source: &source::Source) -> bool {
    [
        "pg_create_logical_replication_slot(",
        "pg_drop_replication_slot(",
        "replication=database",
    ]
    .iter()
    .any(|op| {
        source
            .literals
            .iter()
            .any(|literal| literal.to_ascii_lowercase().contains(op))
    })
}

#[test]
fn worker_closure_does_not_own_replication_or_slot_cleanup() {
    let closure = repo::normal_closure("zeroship-worker");
    let mut scanned = BTreeSet::new();
    for package in repo::workspace() {
        if !closure.contains(package["name"].as_str().unwrap()) {
            continue;
        }
        for target in package["targets"].as_array().unwrap() {
            if !target["kind"]
                .as_array()
                .unwrap()
                .iter()
                .any(|v| v == "lib" || v == "bin" || v == "proc-macro")
            {
                continue;
            }
            let entry = std::path::Path::new(target["src_path"].as_str().unwrap());
            if !entry.starts_with(repo::root().join("crates")) {
                continue;
            }
            for (path, source) in source::module_tree(entry) {
                assert!(
                    !replication_use(&source),
                    "worker dependency owns replication: {}",
                    path.display()
                );
                scanned.insert(path);
            }
        }
    }
    assert!(scanned.len() >= 200, "worker closure scan lost its corpus");
    let main = source::parse(&repo::read("crates/zeroship-worker/src/main.rs"));
    assert!(!main.names_any(&["slot_reaper", "run_server_with_slot_reaper"]));
    assert!(replication_use(&source::parse(
        "fn f() { execute(\"SELECT pg_create_logical_replication_slot('slot', 'pgoutput')\"); }"
    )));
    assert!(replication_use(&source::parse(
        "fn f() { connect(\"replication=database\"); }"
    )));
    assert!(!replication_use(&source::parse(
        "#[cfg(test)] fn f() { connect(\"replication=database\"); }"
    )));
}

fn role_attributes(input: &str) -> Option<bool> {
    let statement = Regex::new(r#"(?i)ALTER\s+ROLE\s+zeroship_worker\s+([^;"\n]+)"#).unwrap();
    let attribute = Regex::new(r"(?i)\b(NO)?REPLICATION\b").unwrap();
    statement
        .captures_iter(input)
        .filter_map(|s| {
            attribute
                .captures_iter(&s[1])
                .last()
                .map(|v| v.get(1).is_none())
        })
        .last()
}

#[test]
fn worker_role_and_boot_checks_refuse_replication_authority() {
    assert!(repo::root()
        .join("db/migrations-ts/20260910000100_cdc_relay_authority.ts")
        .exists());
    let mut attributes = Vec::new();
    for file in repo::files("db/migrations-ts", &["ts"]) {
        if let Some(value) = role_attributes(&std::fs::read_to_string(file).unwrap()) {
            attributes.push(value);
        }
    }
    assert!(
        !attributes.is_empty(),
        "migration authority scan must rule on a role statement"
    );
    assert_eq!(
        attributes.last(),
        Some(&false),
        "the final migration must refuse replication"
    );
    assert_eq!(
        role_attributes("ALTER ROLE zeroship_worker REPLICATION;"),
        Some(true)
    );
    assert_eq!(
        role_attributes("ALTER ROLE zeroship_worker NOREPLICATION NOBYPASSRLS;"),
        Some(false)
    );
    let body = source::parse(&repo::read("crates/zeroship-worker/src/db_posture.rs"));
    assert!(body.contains("if posture.replication || posture.bypass_rls"));
    assert!(!body.contains("!posture.replication"));
    assert!(
        body.tokens.matches("return Err").count() >= 6,
        "boot refusal corpus disappeared"
    );
}
