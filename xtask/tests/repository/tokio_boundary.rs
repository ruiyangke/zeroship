use super::repo;
use serde_json::Value;
use std::collections::BTreeSet;
use std::path::Path;
use std::process::Command;

// Update these sets and the AGENTS.md invariant together when the accepted
// transitive dependency changes. They describe linked dependencies, not which
// runtime drives the application's I/O.
const CARRIERS: &[&str] = &["cyper", "cyper-core", "hyper", "hyper-util"];
const ENTRYPOINTS: &[&str] = &[
    "compio-s3",
    "zeroship-auth",
    "zeroship-control",
    "zeroship-core",
    "zeroship-gateway",
    "zeroship-mailer",
    "zeroship-runtime",
    "zeroship-worker",
    "zeroship-workflow",
];

fn is_tokio(name: &str) -> bool {
    name == "tokio" || name.starts_with("tokio-")
}

fn forbidden_declarations(package: &Value) -> Vec<String> {
    package["dependencies"]
        .as_array()
        .expect("package dependencies")
        .iter()
        .filter(|dependency| {
            is_tokio(
                dependency["name"]
                    .as_str()
                    .expect("dependency package name"),
            ) && dependency["kind"] != "dev"
        })
        .map(|dependency| {
            format!(
                "{}: {} (kind {}, target {})",
                package["name"], dependency["name"], dependency["kind"], dependency["target"]
            )
        })
        .collect()
}

fn root_declarations(source: &str) -> Result<(usize, Vec<String>), String> {
    let manifest = source
        .parse::<toml_edit::DocumentMut>()
        .map_err(|error| error.to_string())?;
    let dependencies = manifest
        .get("workspace")
        .and_then(|workspace| workspace.get("dependencies"))
        .and_then(toml_edit::Item::as_table_like)
        .ok_or("missing workspace.dependencies table")?;
    if dependencies.is_empty() {
        return Err("empty workspace.dependencies table".into());
    }
    let forbidden = dependencies
        .iter()
        .filter(|(alias, dependency)| {
            is_tokio(alias)
                || dependency
                    .get("package")
                    .and_then(toml_edit::Item::as_str)
                    .is_some_and(is_tokio)
        })
        .map(|(alias, _)| alias.to_owned())
        .collect();
    Ok((dependencies.len(), forbidden))
}

// Cargo metadata's resolve graph can include optional edges that cargo tree
// excludes under the active feature resolution. Keep Cargo responsible for
// selecting normal edges; do not approximate that selection from metadata.
fn tokio_tree(root: &Path, all_features: bool) -> Result<String, String> {
    let mut command = Command::new(env!("CARGO"));
    command.current_dir(root).args([
        "tree",
        "--workspace",
        "--locked",
        "-e",
        "normal",
        "-i",
        "tokio",
        "--prefix",
        "none",
        "--format",
        "{p}",
        "--color",
        "never",
    ]);
    if all_features {
        command.arg("--all-features");
    }
    let output = command.output().map_err(|error| error.to_string())?;
    if !output.status.success() {
        return Err(format!(
            "cargo tree (all_features={all_features}) failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    String::from_utf8(output.stdout).map_err(|error| error.to_string())
}

fn tree_packages(output: &str) -> Result<BTreeSet<String>, String> {
    let row = regex::Regex::new(r"^([A-Za-z0-9_-]+) v[0-9][^ ]*(?: \(.+\))?$").unwrap();
    let mut packages = BTreeSet::new();
    for line in output.lines() {
        let line = line.strip_suffix(" (*)").unwrap_or(line);
        let capture = row
            .captures(line)
            .ok_or_else(|| format!("unexpected cargo tree row: {line:?}"))?;
        packages.insert(capture[1].to_owned());
    }
    if !packages.remove("tokio") {
        return Err("cargo tree did not report the required tokio root".into());
    }
    Ok(packages)
}

fn entrypoints(packages: &[&Value], carriers: &BTreeSet<String>) -> BTreeSet<String> {
    packages
        .iter()
        .filter(|package| {
            package["dependencies"]
                .as_array()
                .expect("dependencies")
                .iter()
                .any(|dependency| {
                    carriers.contains(dependency["name"].as_str().expect("dependency name"))
                })
        })
        .map(|package| package["name"].as_str().expect("package name").to_owned())
        .collect()
}

#[test]
fn tokio_declarations_remain_dev_only_and_member_owned() {
    let packages = repo::workspace();
    assert!(packages.len() >= 20, "workspace scan lost its packages");
    let forbidden: Vec<_> = packages
        .iter()
        .flat_map(|p| forbidden_declarations(p))
        .collect();
    assert!(
        forbidden.is_empty(),
        "non-dev tokio declarations: {forbidden:?}"
    );

    let (examined, forbidden) = root_declarations(&repo::read("Cargo.toml")).unwrap();
    assert!(examined >= 40, "root dependency scan lost its entries");
    assert!(
        forbidden.is_empty(),
        "workspace tokio declarations: {forbidden:?}"
    );
}

#[test]
fn accepted_transitive_tokio_boundary_is_unchanged() {
    let packages = repo::workspace();
    assert!(packages.len() >= 20, "entrypoint scan lost its packages");
    let workspace_names: BTreeSet<_> = packages
        .iter()
        .map(|package| package["name"].as_str().unwrap().to_owned())
        .collect();
    let mut reachers = BTreeSet::new();
    for all_features in [false, true] {
        let output = tokio_tree(&repo::root(), all_features).unwrap();
        let selected = tree_packages(&output).unwrap();
        assert!(
            !selected.is_empty(),
            "tokio reverse dependency scan is empty"
        );
        reachers.extend(selected);
    }
    assert!(
        reachers.len() >= 8,
        "tokio reachability scan lost its packages"
    );
    let carriers = reachers.difference(&workspace_names).cloned().collect();
    assert_eq!(
        carriers,
        CARRIERS.iter().map(|name| (*name).to_owned()).collect(),
        "transitive tokio carriers changed; review this boundary and AGENTS.md together"
    );
    assert_eq!(
        entrypoints(&packages, &carriers),
        ENTRYPOINTS.iter().map(|name| (*name).to_owned()).collect(),
        "workspace tokio entrypoints changed; review this boundary and AGENTS.md together"
    );
}

#[test]
fn declaration_checks_use_package_identity_and_dependency_kind() {
    for name in ["tokio", "tokio-postgres"] {
        for target in [Value::Null, Value::from("cfg(windows)")] {
            for kind in [Value::Null, Value::from("build"), Value::from("dev")] {
                let package = serde_json::json!({
                    "name": "consumer",
                    "dependencies": [{ "name": name, "rename": "oracle", "kind": kind, "target": target }]
                });
                assert_eq!(forbidden_declarations(&package).is_empty(), kind == "dev");
            }
        }
    }
    let package = serde_json::json!({
        "name": "consumer", "dependencies": [{"name": "compio", "kind": null}]
    });
    assert!(forbidden_declarations(&package).is_empty());
}

#[test]
fn root_dependency_check_parses_toml_spellings_and_aliases() {
    for declaration in [
        "[workspace.dependencies] # shared versions\ntokio={version='1'}",
        "[workspace.dependencies]\n\"tokio\"='1'",
        "[workspace.dependencies.tokio]\nversion='1'",
        "[workspace.dependencies]\ntokio.version='1'",
        "['workspace'.dependencies]\ntokio='1'",
        "[workspace]\ndependencies.tokio='1'",
        "[workspace]\ndependencies={tokio='1'}",
        "[workspace.dependencies.rt]\npackage='tokio'\nversion='1'",
        "[workspace.dependencies]\nrt.package='tokio'\nrt.version='1'",
        "[workspace.dependencies]\nrt={package='tokio-postgres', version='1'}",
        "[workspace.dependencies]\n\"to\\u006bio\"='1'",
    ] {
        let (examined, forbidden) = root_declarations(declaration).unwrap();
        assert_eq!(examined, 1, "{declaration}");
        assert_eq!(forbidden.len(), 1, "missed declaration: {declaration}");
    }
    let permitted = r#"
        [workspace.dependencies] # tokio is only a comment
        compio = '1'
        [workspace.metadata.notes]
        rationale = """tokio is permitted in member dev-dependencies"""
    "#;
    assert_eq!(root_declarations(permitted).unwrap(), (1, Vec::new()));
    for invalid in ["", "[workspace.dependencies]", "[workspace.dependencies"] {
        assert!(root_declarations(invalid).is_err(), "accepted {invalid:?}");
    }
}

#[test]
fn tree_reader_rejects_empty_missing_root_and_unrecognized_output() {
    let output = "tokio v1.0.0\nhyper v1.0.0\nconsumer v0.1.0 (/a path)\nhyper v1.0.0 (*)\n";
    assert_eq!(
        tree_packages(output).unwrap(),
        BTreeSet::from(["hyper".into(), "consumer".into()])
    );
    for invalid in [
        "",
        "hyper v1.0.0\n",
        "tokio v1.0.0\nwarning: unexpected output\n",
    ] {
        assert!(tree_packages(invalid).is_err(), "accepted {invalid:?}");
    }
}

#[test]
fn carrier_entrypoints_include_dev_build_target_and_renamed_dependencies() {
    let carriers = BTreeSet::from(["cyper".into()]);
    for kind in [Value::Null, Value::from("build"), Value::from("dev")] {
        let package = serde_json::json!({
            "name": "consumer",
            "dependencies": [{ "name": "cyper", "rename": "http", "kind": kind, "target": "cfg(windows)" }]
        });
        assert_eq!(
            entrypoints(&[&package], &carriers),
            BTreeSet::from(["consumer".into()])
        );
        assert!(entrypoints(&[&package], &BTreeSet::from(["unrelated".into()])).is_empty());
    }
}

#[test]
fn cargo_selects_activated_normal_edges_without_dev_or_build_edges() {
    let fixture = tempfile::tempdir().unwrap();
    let root = fixture.path();
    std::fs::write(
        root.join("Cargo.toml"),
        "[workspace]\nmembers=['app']\nexclude=['tokio', 'carrier', 'oracle']\nresolver='3'\n",
    )
    .unwrap();
    for (name, dependencies) in [
        ("tokio", ""),
        (
            "carrier",
            "[dependencies]\ntokio={path='../tokio', optional=true}\n",
        ),
        ("oracle", "[dependencies]\ntokio={path='../tokio'}\n"),
        (
            "app",
            "[dependencies]\ntokio={path='../tokio'}\ncarrier={path='../carrier'}\n\
            [dev-dependencies]\noracle={path='../oracle'}\n\
            [build-dependencies]\noracle={path='../oracle'}\n\
            [features]\nruntime=['carrier/tokio']\n",
        ),
    ] {
        let package = root.join(name);
        std::fs::create_dir_all(package.join("src")).unwrap();
        std::fs::write(package.join("src/lib.rs"), "").unwrap();
        std::fs::write(
            package.join("Cargo.toml"),
            format!("[package]\nname='{name}'\nversion='0.1.0'\nedition='2021'\n{dependencies}"),
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
    let default = tree_packages(&tokio_tree(root, false).unwrap()).unwrap();
    let all = tree_packages(&tokio_tree(root, true).unwrap()).unwrap();
    assert_eq!(default, BTreeSet::from(["app".into()]));
    assert_eq!(all, BTreeSet::from(["app".into(), "carrier".into()]));
    std::fs::remove_file(root.join("Cargo.lock")).unwrap();
    assert!(
        tokio_tree(root, false).is_err(),
        "Cargo failure was accepted"
    );
}
