//! Repository-wide build-input, dependency-boundary and toolchain checks.
//!
//! Declaring each `mod` below is load-bearing: a module missing from this list
//! is a suite that silently stops running.

mod build_inputs;
mod crypto_library;
mod deploy_image;
mod jwt_backend;
mod shards;
mod testkit_boundaries;
mod tls_provider;
mod tokio_boundary;
mod workflow_crates;

use crate::architecture::repo;
use serde_json::Value;
use std::collections::BTreeSet;
use std::path::Path;
use std::process::Command;

/// A workspace under `root` whose `members` are named among `packages`; the
/// rest are path dependencies outside it. Each package is `(name, manifest
/// after [package], files)`, and every file holds an empty `main`. The lockfile
/// is generated offline, so a resolve over it needs nothing from a registry.
fn write_fixture_workspace(root: &Path, members: &[&str], packages: &[(&str, &str, &[&str])]) {
    let quote = |names: Vec<&str>| {
        names
            .iter()
            .map(|name| format!("'{name}'"))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let outside = packages
        .iter()
        .map(|(name, ..)| *name)
        .filter(|name| !members.contains(name))
        .collect();
    std::fs::write(
        root.join("Cargo.toml"),
        format!(
            "[workspace]\nmembers=[{}]\nexclude=[{}]\nresolver='3'\n",
            quote(members.to_vec()),
            quote(outside)
        ),
    )
    .unwrap();
    for (name, manifest, files) in packages {
        let package = root.join(name);
        std::fs::create_dir_all(package.join("src")).unwrap();
        for file in *files {
            std::fs::write(package.join(file), "fn main() {}\n").unwrap();
        }
        std::fs::write(
            package.join("Cargo.toml"),
            format!("[package]\nname='{name}'\nversion='0.1.0'\nedition='2021'\n{manifest}"),
        )
        .unwrap();
    }
    let output = Command::new(env!("CARGO"))
        .current_dir(root)
        .args(["generate-lockfile", "--offline"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn dev_only_feature_routes(package: &Value) -> (usize, Vec<String>) {
    let dependencies = package["dependencies"].as_array().expect("dependencies");
    let names = |dev| -> BTreeSet<&str> {
        dependencies
            .iter()
            .filter(|dependency| (dependency["kind"] == "dev") == dev)
            .map(|dependency| {
                dependency["rename"]
                    .as_str()
                    .or_else(|| dependency["name"].as_str())
                    .expect("dependency name")
            })
            .collect()
    };
    let dev = names(true);
    let shipped = names(false);
    let mut examined = 0;
    let mut forbidden = Vec::new();
    for (feature, entries) in package["features"].as_object().expect("features") {
        for entry in entries.as_array().expect("feature entries") {
            let entry = entry.as_str().expect("feature entry");
            let Some((dependency, _)) = entry.split_once('/') else {
                continue;
            };
            examined += 1;
            let dependency = dependency.trim_end_matches('?');
            if dev.contains(dependency) && !shipped.contains(dependency) {
                forbidden.push(format!("{feature} -> {entry}"));
            }
        }
    }
    (examined, forbidden)
}

#[test]
fn workspace_features_do_not_activate_dev_only_dependencies() {
    let packages = repo::workspace();
    assert!(packages.len() >= 10, "workspace scan lost its packages");
    let mut examined = 0;
    let mut violations = Vec::new();
    for package in packages {
        let (routes, forbidden) = dev_only_feature_routes(package);
        examined += routes;
        violations.extend(
            forbidden
                .into_iter()
                .map(|route| format!("{}: {route}", package["name"])),
        );
    }
    assert!(examined >= 10, "feature scan lost its dependency routes");
    assert!(
        violations.is_empty(),
        "dev-only feature routes: {violations:?}"
    );
}

#[test]
fn feature_routes_distinguish_dependency_kinds_aliases_and_weak_activation() {
    let mut package = serde_json::json!({
        "dependencies": [{ "name": "driver-oracle", "rename": "oracle", "kind": "dev" }],
        "features": { "codec": ["oracle/json", "oracle?/uuid", "dep:optional", "local"] }
    });
    let (examined, forbidden) = dev_only_feature_routes(&package);
    assert_eq!(examined, 2);
    assert_eq!(forbidden, ["codec -> oracle/json", "codec -> oracle?/uuid"]);

    for kind in [Value::Null, Value::from("build")] {
        package["dependencies"][0]["kind"] = kind;
        assert!(dev_only_feature_routes(&package).1.is_empty());
    }
    package["dependencies"]
        .as_array_mut()
        .unwrap()
        .push(serde_json::json!({
            "name": "driver-oracle", "rename": "oracle", "kind": "dev"
        }));
    assert!(dev_only_feature_routes(&package).1.is_empty());

    package["dependencies"] = serde_json::json!([
        { "name": "oracle", "kind": "dev", "target": "cfg(unix)" }
    ]);
    assert_eq!(dev_only_feature_routes(&package).1.len(), 2);
}
