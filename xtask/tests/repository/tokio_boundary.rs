use crate::architecture::repo;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::process::Command;

// Update these sets and the zero-tokio invariant together when the accepted
// transitive dependency changes. They describe linked dependencies, not which
// runtime drives the application's I/O - SHIPPED_TOKIO_FEATURES below is what
// settles that.
const CARRIERS: &[&str] = &["cyper", "cyper-core", "hyper", "hyper-util"];

// Workspace members that REACH tokio in a SHIPPED binary's resolved graph.
//
// Reachability, not declaration, and only through packages that ship a binary.
// The dev-only testkit members are not in any shipped binary's normal closure,
// so the testcontainers -> bollard -> tokio path they carry is deliberately
// absent here; the `--workspace` tree that would include it is the control in
// `shipped_builds_compile_no_tokio_runtime`, not the subject.
//
// The members absent here are the ones this pin protects - the leaves that must
// stay clean, `zeroship-workflow-schema` and `zeroship-id` among them.
const REACHERS: &[&str] = &[
    "compio-s3",
    "zeroship-auth",
    "zeroship-authn",
    "zeroship-bundle",
    "zeroship-cli",
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

/// The manifest classes a package gives its targets, keyed by target name.
fn target_classes(package: &Value) -> BTreeMap<String, String> {
    package["metadata"]["zeroship-config"]["targets"]
        .as_array()
        .map(|targets| {
            targets
                .iter()
                .filter_map(|target| {
                    Some((
                        target["target"].as_str()?.to_owned(),
                        target["class"].as_str()?.to_owned(),
                    ))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Whether every target this package classifies is `test-dev-tool`, i.e. the
/// package itself ships nothing. A package with no classification at all is not
/// exempt: only an explicit test-dev-tool class buys the exemption.
fn is_test_dev_tool_package(package: &Value) -> bool {
    let name = package["name"].as_str().expect("package name");
    let classes = target_classes(package);
    classes.get(name).is_some_and(|class| class == "test-dev-tool")
}

/// The bin targets this package would ship: those its manifest does not
/// classify `test-dev-tool`.
fn shipped_bin_targets(package: &Value) -> Vec<String> {
    let classes = target_classes(package);
    package["targets"]
        .as_array()
        .expect("package targets")
        .iter()
        .filter(|target| {
            target["kind"]
                .as_array()
                .is_some_and(|kinds| kinds.iter().any(|kind| kind == "bin"))
        })
        .filter_map(|target| target["name"].as_str())
        .filter(|name| classes.get(*name).map(String::as_str) != Some("test-dev-tool"))
        .map(str::to_owned)
        .collect()
}

/// The workspace packages that ship at least one binary.
fn shipped_packages() -> Vec<&'static Value> {
    let packages: Vec<_> = repo::workspace()
        .into_iter()
        .filter(|package| !shipped_bin_targets(package).is_empty())
        .collect();
    assert!(
        packages.len() >= 5,
        "shipped-binary corpus disappeared: {}",
        packages.len()
    );
    packages
}

fn forbidden_declarations(package: &Value) -> Vec<String> {
    if is_test_dev_tool_package(package) {
        return Vec::new();
    }
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

/// `cargo tree -p package -i tokio` over normal edges. Cargo is responsible for
/// selecting activated edges; do not approximate that selection from metadata.
fn package_tokio_tree(root: &Path, package: &str, all_features: bool) -> Result<String, String> {
    let mut command = Command::new(env!("CARGO"));
    command.current_dir(root).args([
        "tree",
        "--locked",
        "-p",
        package,
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
            "cargo tree -p {package} (all_features={all_features}) failed: {}",
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
fn tokio_feature_tree(root: &Path, packages: &[&str], include_dev: bool) -> Result<String, String> {
    let edges = if include_dev { "features" } else { "features,normal" };
    let mut command = Command::new(env!("CARGO"));
    command.current_dir(root).arg("tree");
    if packages.is_empty() {
        command.arg("--workspace");
    }
    for package in packages {
        command.args(["-p", package]);
    }
    command.args(["--locked", "-e", edges, "--color", "never"]);
    let output = command.output().map_err(|error| error.to_string())?;
    if !output.status.success() {
        return Err(format!(
            "cargo tree (features, packages={packages:?}, include_dev={include_dev}) failed: {}",
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

/// The tokio features a shipped package activates, or none when it does not
/// reach tokio at all.
fn shipped_package_features(root: &Path, package: &str) -> BTreeSet<String> {
    if !repo::normal_closure(package).contains("tokio") {
        return BTreeSet::new();
    }
    let output = tokio_feature_tree(root, &[package], false)
        .unwrap_or_else(|error| panic!("feature tree for {package}: {error}"));
    tokio_features(&output)
        .unwrap_or_else(|error| panic!("{package} reaches tokio but has no feature rows: {error}"))
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
        "non-dev tokio declarations outside a test-dev-tool package: {forbidden:?}"
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
    let mut contributing = 0;
    for package in shipped_packages() {
        let name = package["name"].as_str().unwrap();
        if !repo::normal_closure(name).contains("tokio") {
            continue;
        }
        contributing += 1;
        for all_features in [false, true] {
            let output = package_tokio_tree(&repo::root(), name, all_features).unwrap();
            let selected = tree_packages(&output).unwrap();
            assert!(
                !selected.is_empty(),
                "tokio reverse dependency scan for {name} is empty"
            );
            reachers.extend(selected);
        }
    }
    assert!(
        contributing >= 5 && reachers.len() >= 8,
        "tokio reachability scan lost its packages: {contributing} packages, \
         {} reachers",
        reachers.len()
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
/// The subject is the union of every shipped package's normal features, not the
/// whole workspace: the dev-only testkit members normally carry
/// `testcontainers -> bollard -> tokio rt`, and they are not shipped.
///
/// The control is the second half, and without it the first half is vacuous: a
/// reader that silently matched nothing would report "no runtime features" over
/// an empty string. Admitting dev edges MUST surface `rt`, because
/// `testcontainers` -> `bollard` -> `hyper-util` needs a real tokio runtime to
/// reach the Docker daemon. Same reader, same regex, one flag apart.
#[test]
fn shipped_builds_compile_no_tokio_runtime() {
    let mut shipped = BTreeSet::new();
    let mut contributing = 0;
    for package in shipped_packages() {
        let name = package["name"].as_str().unwrap();
        let features = shipped_package_features(&repo::root(), name);
        if !features.is_empty() {
            contributing += 1;
            shipped.extend(features);
        }
    }
    assert!(
        contributing >= 5 && !shipped.is_empty(),
        "shipped feature scan read no tokio features"
    );
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

    let with_dev = tokio_features(&tokio_feature_tree(&repo::root(), &[], true).unwrap()).unwrap();
    for feature in RUNTIME_FEATURES {
        assert!(
            with_dev.contains(*feature),
            "dev edges do not enable tokio {feature:?}, so the shipped assertion \
             above is reading a feature set this reader cannot see"
        );
    }
}

/// The packages `deploy/Dockerfile` builds the shipped binaries from, read off
/// its `RUN cargo build --release` invocation and the `\` continuations after
/// it.
///
/// # Errors
/// When there is no such invocation, it selects no package, or it builds the
/// whole workspace.
pub(super) fn dockerfile_build(source: &str) -> Result<Vec<String>, String> {
    let mut lines = source.lines().skip_while(|line| !line.trim_start().starts_with("RUN cargo build"));
    let first = lines
        .next()
        .ok_or("the Dockerfile has no `RUN cargo build` invocation")?;
    let mut invocation = first.trim().to_owned();
    let mut continued = first.trim_end().ends_with('\\');
    for line in lines {
        if !continued {
            break;
        }
        invocation.push(' ');
        invocation.push_str(line.trim());
        continued = line.trim_end().ends_with('\\');
    }
    let words: Vec<&str> = invocation
        .split_whitespace()
        .filter(|word| *word != "\\")
        .collect();
    if words.iter().any(|word| *word == "--workspace" || *word == "--all") {
        return Err(format!(
            "the Dockerfile builds the whole workspace, which unifies the dev-only \
             testkits' tokio runtime into the shipped binaries: {invocation}"
        ));
    }
    let packages: Vec<String> = words
        .windows(2)
        .filter(|pair| pair[0] == "-p" || pair[0] == "--package")
        .map(|pair| pair[1].to_owned())
        .collect();
    if packages.is_empty() {
        return Err(format!("the Dockerfile build selects no package: {invocation}"));
    }
    Ok(packages)
}

/// The build that produces the deployed binaries compiles no tokio runtime.
///
/// The per-package scan above unions what each shipped package enables alone;
/// the image is built by ONE cargo invocation over several packages, whose
/// features cargo unifies across the selection. This reads that exact
/// selection off `deploy/Dockerfile` and resolves it the way the build does.
///
/// The control is the reason the build selects packages at all: the same
/// reader over `--workspace` normal edges MUST surface tokio `rt`, because the
/// dev-only testkits are workspace members whose testcontainers dependency
/// needs a real runtime, and a workspace build unifies it into every member it
/// builds.
#[test]
fn the_dockerfile_build_compiles_no_tokio_runtime() {
    let packages = dockerfile_build(&repo::read("deploy/Dockerfile")).unwrap();
    assert!(
        packages.len() >= 5,
        "the Dockerfile build selection lost its packages: {packages:?}"
    );
    let shipped: BTreeSet<String> = shipped_packages()
        .iter()
        .map(|package| package["name"].as_str().unwrap().to_owned())
        .collect();
    for package in &packages {
        assert!(
            shipped.contains(package),
            "the Dockerfile builds {package}, which ships no binary this scan knows"
        );
    }
    let selection: Vec<&str> = packages.iter().map(String::as_str).collect();
    let built = tokio_features(&tokio_feature_tree(&repo::root(), &selection, false).unwrap())
        .unwrap();
    let runtime: Vec<_> = RUNTIME_FEATURES
        .iter()
        .filter(|feature| built.contains(**feature))
        .collect();
    assert!(
        runtime.is_empty(),
        "the Dockerfile build enables tokio {runtime:?}, so the deployed binaries \
         carry a tokio runtime; review the zero-tokio invariant in AGENTS.md"
    );
    let allowed: BTreeSet<String> = SHIPPED_TOKIO_FEATURES
        .iter()
        .map(|name| (*name).to_owned())
        .collect();
    assert!(
        built.is_subset(&allowed),
        "the Dockerfile build enables tokio features outside the shipped set: {:?}",
        built.difference(&allowed).collect::<Vec<_>>()
    );

    let workspace = tokio_features(&tokio_feature_tree(&repo::root(), &[], false).unwrap())
        .unwrap();
    assert!(
        RUNTIME_FEATURES.iter().any(|feature| workspace.contains(*feature)),
        "a --workspace build does not unify a tokio runtime in, so this control \
         cannot show the reader sees one; it found {workspace:?}"
    );
}

#[test]
fn the_dockerfile_reader_takes_the_selection_and_refuses_a_workspace_build() {
    let image = "FROM rust AS builder\nRUN cargo build --release \\\n    -p zeroship-control \\\n    -p zeroship-gateway\n\nFROM ubuntu\nRUN apt-get -p nope\n";
    assert_eq!(
        dockerfile_build(image).unwrap(),
        ["zeroship-control", "zeroship-gateway"]
    );
    for refused in [
        "RUN cargo build --release --workspace\n",
        "RUN cargo build --release \\\n    --workspace \\\n    -p zeroship-control\n",
        "RUN cargo build --release\n",
        "FROM rust\nRUN make\n",
    ] {
        assert!(dockerfile_build(refused).is_err(), "accepted {refused:?}");
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
fn declaration_check_exempts_only_a_classified_test_dev_tool() {
    let declared = serde_json::json!({
        "name": "zeroship-testkit",
        "dependencies": [{"name": "tokio", "kind": null, "target": null}],
        "metadata": {"zeroship-config": {"targets": [
            {"target": "zeroship-testkit", "class": "test-dev-tool"}
        ]}}
    });
    assert!(forbidden_declarations(&declared).is_empty());
    let shipped = serde_json::json!({
        "name": "zeroship-worker",
        "dependencies": [{"name": "tokio", "kind": null, "target": null}],
        "metadata": {"zeroship-config": {"targets": [
            {"target": "zeroship-worker", "class": "platform"}
        ]}}
    });
    assert_eq!(forbidden_declarations(&shipped).len(), 1);
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
fn shipped_target_classification_ignores_test_dev_tool_bins() {
    let mixed = serde_json::json!({
        "name": "zeroship-control",
        "targets": [
            {"name": "zeroship-control", "kind": ["bin"]},
            {"name": "zeroship-mock-stripe", "kind": ["bin"]},
            {"name": "zeroship-control", "kind": ["lib"]}
        ],
        "metadata": {"zeroship-config": {"targets": [
            {"target": "zeroship-control", "class": "platform"},
            {"target": "zeroship-mock-stripe", "class": "test-dev-tool"}
        ]}}
    });
    assert_eq!(shipped_bin_targets(&mixed), ["zeroship-control"]);
    assert!(!is_test_dev_tool_package(&mixed));
    let only_dev = serde_json::json!({
        "name": "zeroship-testkit",
        "targets": [{"name": "zeroship-testkit", "kind": ["lib"]}],
        "metadata": {"zeroship-config": {"targets": [
            {"target": "zeroship-testkit", "class": "test-dev-tool"}
        ]}}
    });
    assert!(shipped_bin_targets(&only_dev).is_empty());
    assert!(is_test_dev_tool_package(&only_dev));
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
    let default = tree_packages(&package_tokio_tree(root, "app", false).unwrap()).unwrap();
    let all = tree_packages(&package_tokio_tree(root, "app", true).unwrap()).unwrap();
    assert_eq!(default, BTreeSet::from(["app".into()]));
    assert_eq!(all, BTreeSet::from(["app".into(), "carrier".into()]));
    std::fs::remove_file(root.join("Cargo.lock")).unwrap();
    assert!(
        package_tokio_tree(root, "app", false).is_err(),
        "Cargo failure was accepted"
    );
    assert!(
        tokio_feature_tree(root, &["app"], false).is_err(),
        "Cargo failure was accepted by the feature reader"
    );
}