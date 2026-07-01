use std::path::Path;

fn parse_manifest(path: &Path) -> toml::Value {
    let raw = std::fs::read_to_string(path)
        .unwrap_or_else(|e| panic!("read manifest {}: {e}", path.display()));
    raw.parse::<toml::Value>()
        .unwrap_or_else(|e| panic!("parse manifest {}: {e}", path.display()))
}

fn dep_features(manifest: &toml::Value, section: &str, dep: &str) -> Vec<String> {
    manifest
        .get(section)
        .and_then(|deps| deps.get(dep))
        .and_then(|entry| entry.get("features"))
        .and_then(toml::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(toml::Value::as_str)
        .map(str::to_string)
        .collect()
}

// The default-build symbol absence check is the compile_fail doctest in
// src/lib.rs. These integration tests keep the feature-unification side honest.
#[test]
fn shared_infra_crates_do_not_enable_standalone_cli() {
    let manifest_dir = Path::new(env!("CARGO_MANIFEST_DIR"));
    for relative in [
        "../control/Cargo.toml",
        "../plugin-db/Cargo.toml",
        "../schema-authority-e2e/Cargo.toml",
    ] {
        let path = manifest_dir.join(relative);
        let manifest = parse_manifest(&path);
        for section in ["dependencies", "dev-dependencies", "build-dependencies"] {
            let features = dep_features(&manifest, section, "zeroship-migrate");
            assert!(
                !features.iter().any(|f| f == "standalone-cli"),
                "{} enables zeroship-migrate/standalone-cli in {section}: {features:?}",
                path.display()
            );
        }
    }
}

#[test]
fn root_workspace_uses_resolver_3_for_feature_isolation() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../Cargo.toml");
    let manifest = parse_manifest(&root);
    let resolver = manifest
        .get("workspace")
        .and_then(|workspace| workspace.get("resolver"))
        .and_then(toml::Value::as_str);
    assert_eq!(resolver, Some("3"));

    let features = manifest
        .get("workspace")
        .and_then(|workspace| workspace.get("dependencies"))
        .and_then(|deps| deps.get("zeroship-migrate"))
        .and_then(|entry| entry.get("features"))
        .and_then(toml::Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(toml::Value::as_str)
        .collect::<Vec<_>>();
    assert!(
        !features.iter().any(|f| *f == "standalone-cli"),
        "workspace zeroship-migrate dependency must not enable standalone-cli: {features:?}"
    );
}
