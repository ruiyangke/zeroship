use crate::{Result, cargo, checked};
use serde_json::Value;
use std::process::Command;

const PACKAGES: &[&str] = &[
    "zeroship-data-sql",
    "zeroship-data-macros",
    "zeroship-data-orm",
    "zeroship-data-v8",
    "zeroship-data-cdc-wire",
    "zeroship-data-cdc-server",
];

pub fn run(filter: Option<&str>) -> Result<()> {
    if filter.is_none() {
        architecture()?;
    }
    checked(
        cargo().args(["nextest", "--version"]),
        "cargo-nextest is required",
    )?;
    check_snapshot_clients()?;
    let output = cargo()
        .args(["metadata", "--no-deps", "--format-version", "1"])
        .output()?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).into_owned().into());
    }
    validate_targets(&serde_json::from_slice(&output.stdout)?)?;
    checked(
        cargo().args(["build", "-p", "zeroship-data-cdc-server"]),
        "build the real CDC relay",
    )?;

    eprintln!("Building the PostgreSQL image used by native fixtures");
    let _ = crate::postgres_image::build()?;
    crate::cancelled()?;
    let mut nextest = cargo();
    nextest.args([
        "nextest",
        "run",
        "--profile",
        "data",
        "--ignore-default-filter",
        "--no-tests",
        "fail",
    ]);
    packages(&mut nextest);
    if let Some(filter) = filter {
        nextest.args(["--filter-expr", filter]);
    }
    nextest.env("RUST_MIN_STACK", "33554432");
    // Preserve a failed nextest verdict while still checking Rust documentation.
    let tests = checked(&mut nextest, "nextest data suite");
    let docs = if filter.is_none() {
        let mut command = cargo();
        command.args(["test", "--doc", "--no-fail-fast"]);
        packages(&mut command);
        command.env("RUST_MIN_STACK", "33554432");
        checked(&mut command, "data doctests")
    } else {
        Ok(())
    };
    tests.and(docs)
}

pub fn architecture() -> Result<()> {
    checked(
        cargo().args([
            "test",
            "--manifest-path",
            "xtask/Cargo.toml",
            "--test",
            "data_architecture",
        ]),
        "data architecture and database posture",
    )
}

fn check_snapshot_clients() -> Result<()> {
    let server_major: u32 = include_str!("../../tests/fixtures/postgres/Dockerfile")
        .lines()
        .find_map(|line| line.strip_prefix("FROM pgvector/pgvector:pg"))
        .ok_or("PostgreSQL fixture must declare its pgvector server tag")?
        .parse()?;
    for tool in ["pg_dump", "pg_restore"] {
        let output = Command::new(tool)
            .arg("--version")
            .output()
            .map_err(|error| format!("{tool} is required for snapshot tests: {error}"))?;
        if !output.status.success() {
            return Err(format!("{tool} --version failed: {}", output.status).into());
        }
        let version = String::from_utf8(output.stdout)?;
        if client_major(&version) != Some(server_major) {
            return Err(format!(
                "snapshot tests require {tool} matching PostgreSQL {server_major}; found {}. Put matching clients on PATH",
                version.trim()
            ).into());
        }
        eprintln!("{}", version.trim());
    }
    Ok(())
}

fn client_major(version: &str) -> Option<u32> {
    version
        .split_whitespace()
        .nth(2)?
        .split('.')
        .next()?
        .parse()
        .ok()
}

fn packages(command: &mut Command) {
    for package in PACKAGES {
        command.args(["-p", package]);
    }
}

// Database targets must remain in ordinary package tests. No optional feature
// may turn an unavailable database into an apparently successful run.
fn validate_targets(metadata: &Value) -> Result<()> {
    let packages = metadata["packages"]
        .as_array()
        .ok_or("Cargo metadata has no packages")?;
    for name in PACKAGES {
        let package = packages
            .iter()
            .find(|package| package["name"] == *name)
            .ok_or_else(|| format!("missing required data package: {name}"))?;
        if ["live-db-tests", "test-helpers"]
            .iter()
            .any(|feature| package["features"].get(feature).is_some())
        {
            return Err(format!("{name}: live database tests cannot be feature-gated").into());
        }
        let targets = package["targets"]
            .as_array()
            .ok_or("Cargo package has no targets")?;
        let mut tested = false;
        for target in targets {
            if target["test"] == true {
                tested = true;
                if target["required-features"]
                    .as_array()
                    .is_some_and(|features| !features.is_empty())
                {
                    return Err(format!(
                        "{name}: test target {} requires optional features",
                        target["name"]
                    )
                    .into());
                }
            }
        }
        if !tested {
            return Err(format!("{name}: package exposes no ordinary test target").into());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn metadata() -> Value {
        json!({"packages": PACKAGES.iter().map(|name| json!({
            "name": name, "features": {}, "targets": [{"name": name, "test": true}]
        })).collect::<Vec<_>>()})
    }

    #[test]
    fn required_packages_and_ordinary_test_targets_cannot_disappear() {
        assert!(validate_targets(&metadata()).is_ok());
        let mut missing = metadata();
        missing["packages"].as_array_mut().unwrap().pop();
        assert!(validate_targets(&missing).is_err());
        let mut no_tests = metadata();
        no_tests["packages"][0]["targets"][0]["test"] = json!(false);
        assert!(validate_targets(&no_tests).is_err());
    }

    #[test]
    fn feature_gated_database_tests_are_refused() {
        let mut hidden_package = metadata();
        hidden_package["packages"][0]["features"]["live-db-tests"] = json!([]);
        assert!(validate_targets(&hidden_package).is_err());
        let mut hidden_target = metadata();
        hidden_target["packages"][0]["targets"][0]["required-features"] = json!(["optional-db"]);
        assert!(validate_targets(&hidden_target).is_err());
    }
    #[test]
    fn snapshot_client_version_parsing_rejects_unknown_output() {
        assert_eq!(client_major("pg_dump (PostgreSQL) 16.8"), Some(16));
        assert_eq!(
            client_major("pg_restore (PostgreSQL) 17.10 (Ubuntu)"),
            Some(17)
        );
        assert_eq!(client_major("pg_dump unavailable"), None);
    }
}
