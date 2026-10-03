use crate::architecture::repo;
use serde_json::Value;
use std::collections::BTreeSet;
use std::path::Path;
use std::process::Command;

// Update these sets and the zero-tokio invariant together when the accepted
// transitive dependency changes. They describe linked dependencies, not which
// runtime drives the application's I/O - SHIPPED_TOKIO_FEATURES below is what
// settles that.
const CARRIERS: &[&str] = &["cyper", "cyper-core", "hyper", "hyper-util"];

// Workspace members that REACH tokio in the resolved graph.
//
// Reachability, not declaration. A member that declares a carrier it never
// names links exactly what a member that declares nothing links, so pinning
// declarations made this gate fire on edits that changed no dependency at all.
// It also missed the case that matters more: a member can start reaching tokio
// through a NEW intermediate without declaring a carrier itself.
//
// The members absent here are the ones this pin protects - the leaves that must
// stay clean, `zeroship-workflow-schema` and `zeroship-id` among them.
const REACHERS: &[&str] = &[
    "compio-s3",
    "zeroship-auth",
    "zeroship-authn",
    "zeroship-bundle",
    "zeroship-cli",
    "zeroship-config-contract",
    "zeroship-config-contract-fixtures",
    "zeroship-control",
    "zeroship-core",
    "zeroship-data-cdc-server",
    "zeroship-data-orm",
    "zeroship-data-v8",
    "zeroship-gateway",
    "zeroship-kv-v8",
    "zeroship-mailer",
    "zeroship-metering",
    "zeroship-migrate-server",
    "zeroship-runtime",
    "zeroship-storage",
    "zeroship-storage-v8",
    "zeroship-worker",
    "zeroship-workflow",
    "zeroship-workflow-client",
    "zeroship-workflow-manager",
    "zeroship-workflow-runner",
    "zeroship-workflow-server",
    "zeroship-workflow-v8",
];

// The tokio features a SHIPPED build may enable.
//
// This is the invariant, and it is structural rather than a convention. Tokio's
// `default` is empty and its whole `runtime` module sits inside `cfg_rt!`, so
// without `rt` there is no `tokio::runtime`, no `Runtime::new` and no
// `tokio::spawn` anywhere in a shipped binary. hyper's tokio `net`/`sync` TYPES
// compile; nothing can drive them. `cyper-core` depends on `compio`, which is
// the adapter that drives hyper's protocol state machine on compio I/O.
//
// So a carrier cannot introduce a tokio runtime without adding a feature here,
// which is the failure this pin reports by name.
const SHIPPED_TOKIO_FEATURES: &[&str] =
    &["default", "libc", "mio", "net", "socket2", "sync"];

// Features whose presence means a tokio runtime EXISTS and can be started.
const RUNTIME_FEATURES: &[&str] = &["rt", "rt-multi-thread"];

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

// `-e features,normal` restricts to normal edges, so the answer describes what a
// DEPLOYED binary links. Dropping `normal` admits dev edges, which is how the
// control below proves this reader can see a runtime feature when one is there.
fn tokio_feature_tree(root: &Path, include_dev: bool) -> Result<String, String> {
    let edges = if include_dev { "features" } else { "features,normal" };
    let output = Command::new(env!("CARGO"))
        .current_dir(root)
        .args([
            "tree",
            "--workspace",
            "--locked",
            "-e",
            edges,
            "--color",
            "never",
        ])
        .output()
        .map_err(|error| error.to_string())?;
    if !output.status.success() {
        return Err(format!(
            "cargo tree (features, include_dev={include_dev}) failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    String::from_utf8(output.stdout).map_err(|error| error.to_string())
}

fn tokio_features(output: &str) -> Result<BTreeSet<String>, String> {
    let row = regex::Regex::new(r#"\btokio feature "([a-z0-9_-]+)""#).unwrap();
    let features: BTreeSet<String> = row
        .captures_iter(output)
        .map(|capture| capture[1].to_owned())
        .collect();
    if features.is_empty() {
        return Err("no tokio feature rows in cargo tree output".into());
    }
    Ok(features)
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
    let carriers: BTreeSet<_> = reachers.difference(&workspace_names).cloned().collect();
    assert_eq!(
        carriers,
        CARRIERS.iter().map(|name| (*name).to_owned()).collect(),
        "transitive tokio carriers changed; review this boundary and AGENTS.md together"
    );
    // The workspace half, by reachability. A member entering this set links a
    // tokio-carrying package for the first time, however indirectly.
    assert_eq!(
        reachers
            .intersection(&workspace_names)
            .cloned()
            .collect::<BTreeSet<_>>(),
        REACHERS.iter().map(|name| (*name).to_owned()).collect(),
        "workspace members reaching tokio changed; review this boundary and AGENTS.md together"
    );
}

/// A shipped build compiles no tokio runtime, so nothing can start one.
///
/// This is the assertion the package sets above cannot make. They describe what
/// is LINKED; `cfg_rt!` is what decides whether `tokio::runtime` exists at all.
///
/// The control is the second half, and without it the first half is vacuous: a
/// reader that silently matched nothing would report "no runtime features" over
/// an empty string. Admitting dev edges MUST surface `rt`, because
/// `testcontainers` -> `bollard` -> `hyper-util` needs a real tokio runtime to
/// reach the Docker daemon. Same reader, same regex, one flag apart.
#[test]
fn shipped_builds_compile_no_tokio_runtime() {
    let shipped = tokio_features(&tokio_feature_tree(&repo::root(), false).unwrap()).unwrap();
    let runtime: Vec<_> = RUNTIME_FEATURES
        .iter()
        .filter(|feature| shipped.contains(**feature))
        .collect();
    assert!(
        runtime.is_empty(),
        "a shipped build enables tokio {runtime:?}, so a tokio runtime exists; \
         review the zero-tokio invariant in AGENTS.md"
    );
    assert_eq!(
        shipped,
        SHIPPED_TOKIO_FEATURES
            .iter()
            .map(|name| (*name).to_owned())
            .collect(),
        "shipped tokio features changed; review this boundary and AGENTS.md together"
    );

    let with_dev = tokio_features(&tokio_feature_tree(&repo::root(), true).unwrap()).unwrap();
    for feature in RUNTIME_FEATURES {
        assert!(
            with_dev.contains(*feature),
            "dev edges do not enable tokio {feature:?}, so the shipped assertion \
             above is reading a feature set this reader cannot see"
        );
    }
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

/// The feature reader must find features nested anywhere in the tree, and must
/// REFUSE rather than return an empty set when there is nothing to read - an
/// empty set would read as "no runtime features" and pass the assertion above.
#[test]
fn feature_reader_finds_nested_rows_and_refuses_empty_output() {
    let output = "\
tokio v1.51.1\n\
├── hyper-util v0.1.20\n\
│   ├── tokio feature \"net\"\n\
│   │   └── tokio feature \"rt-multi-thread\"\n\
└── tokio feature \"sync\"\n";
    assert_eq!(
        tokio_features(output).unwrap(),
        BTreeSet::from(["net".into(), "rt-multi-thread".into(), "sync".into()])
    );
    for empty in ["", "tokio v1.51.1\nhyper v1.9.0\n", "no features here"] {
        assert!(tokio_features(empty).is_err(), "accepted {empty:?}");
    }
    // A near miss must not be read as a feature: the crate name is part of the row.
    assert!(tokio_features("├── hyper feature \"client\"\n").is_err());
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
    assert!(
        tokio_feature_tree(root, false).is_err(),
        "Cargo failure was accepted by the feature reader"
    );
}
