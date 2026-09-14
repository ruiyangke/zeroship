use super::repo;
use regex::Regex;

fn mandatory_database_tests(package: &serde_json::Value) -> Result<usize, String> {
    let name = package["name"].as_str().ok_or("package name missing")?;
    if ["live-db-tests", "test-helpers"]
        .iter()
        .any(|feature| package["features"].get(feature).is_some())
    {
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
            ![
                "zeroship-testkit",
                "zeroship-test-support",
                "zeroship-test-fixtures",
                "zeroship-data-fixtures"
            ]
            .iter()
            .any(|name| package["name"] == *name),
            "fixtures belong to source modules: {}",
            package["name"]
        );
        assert!(
            ["live-db-tests", "test-helpers"]
                .iter()
                .all(|feature| package["features"].get(feature).is_none()),
            "{} declares optional live database verification",
            package["name"]
        );
    }
    for name in [
        "zeroship-data-orm",
        "zeroship-data-v8",
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
    hidden["features"]["test-helpers"] = serde_json::json!([]);
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
