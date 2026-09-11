#[allow(dead_code)]
#[path = "architecture/repo.rs"]
mod repo;
#[path = "repository/tokio_boundary.rs"]
mod tokio_boundary;

use serde_json::Value;
use std::collections::BTreeSet;

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
