//! A path package must not dev-depend on a package whose own normal closure
//! reaches it: cargo would then build that package twice when the dev-only
//! consumer's tests compile (once as a plain library for the consumer's own
//! sake, once more as the unit-test binary), and the two copies' types would
//! not unify. A domain type named inside a dev-only helper is what would pull
//! such a normal edge back.
//!
//! The rule is read from `cargo metadata`, not from manifest text: a
//! dependency is identified by its real package name whatever alias a
//! `package =` rename gives it, a target-specific table counts like any
//! other normal edge, and the scan covers every path package `cargo metadata`
//! resolves - a workspace member or a path dependency the root manifest
//! excludes - because the double-copy hazard does not care which one a
//! package is.
//!
//! A testkit's name places it in a layer, and its normal path edges may reach
//! only below that layer. A testkit depends only on crates below every crate
//! whose tests use it, because a crate whose own tests link a crate that links
//! it compiles itself twice and its types stop unifying. The layers, from the
//! bottom:
//!
//! - `zeroship-testkit-server` depends on no workspace crate, so any crate's
//!   tests may use it, a `libs/` driver's included.
//! - `zeroship-testkit` depends on the server layer, the `libs/` drivers and
//!   crates with no workspace dependency of their own.
//! - `zeroship-<area>-testkit` depends on the two layers below it and on
//!   production crates below every crate whose tests use it. It never depends
//!   on another area testkit.
//!
//! Fixtures are never a crate. Adapters typed by a crate whose own unit tests
//! need them are a `macro_rules!` in its area testkit, and every crate that
//! uses them expands it.

use crate::architecture::repo;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

/// The real package names `package` declares on dev edges, under any alias and
/// in any target table.
fn dev_dependencies(package: &Value) -> BTreeSet<String> {
    package["dependencies"]
        .as_array()
        .expect("package dependencies")
        .iter()
        .filter(|dependency| dependency["kind"] == "dev")
        .filter_map(|dependency| dependency["name"].as_str().map(str::to_owned))
        .collect()
}

/// The packages in `packages` that declare a dependency on `name`, on any edge.
fn namers_of<'a>(packages: &[&'a Value], name: &str) -> Vec<&'a Value> {
    packages
        .iter()
        .filter(|candidate| {
            candidate["dependencies"]
                .as_array()
                .expect("package dependencies")
                .iter()
                .any(|dependency| dependency["name"] == name)
        })
        .copied()
        .collect()
}

/// Every path package the graph says exists only for tests: it ships no binary,
/// some package names it, and every package that names it does so on a dev edge
/// or is itself a dev-only testkit.
fn dev_only_by_graph<'a>(packages: &[&'a Value]) -> Vec<&'a Value> {
    let mut dev_only = Vec::new();
    for package in packages {
        let name = package["name"].as_str().expect("package name");
        if repo::ships_a_binary(package) {
            continue;
        }
        let namers = namers_of(packages, name);
        if namers.is_empty() {
            continue;
        }
        let dev_namers_only = namers.iter().all(|namer| {
            namer["dependencies"]
                .as_array()
                .expect("package dependencies")
                .iter()
                .filter(|dependency| dependency["name"] == name)
                .all(|dependency| {
                    dependency["kind"] == "dev"
                        || repo::self_classifies_test_dev_tool(namer)
                })
        });
        if dev_namers_only {
            dev_only.push(*package);
        }
    }
    dev_only
}

/// Each package's consumers that its own normal closure reaches.
fn back_edges(
    packages: &[&Value],
    closure: impl Fn(&str) -> BTreeSet<String>,
) -> (usize, BTreeMap<String, Vec<String>>) {
    let mut examined = 0;
    let mut violations = BTreeMap::new();
    for package in packages {
        let name = package["name"].as_str().unwrap();
        let reached = closure(name);
        let consumers: Vec<String> = packages
            .iter()
            .filter(|candidate| dev_dependencies(candidate).contains(name))
            .map(|candidate| candidate["name"].as_str().unwrap().to_owned())
            .collect();
        examined += consumers.len();
        let crossing: Vec<String> = consumers
            .into_iter()
            .filter(|consumer| reached.contains(consumer))
            .collect();
        if !crossing.is_empty() {
            violations.insert(name.to_owned(), crossing);
        }
    }
    (examined, violations)
}

#[test]
fn no_crate_dev_depends_on_a_package_that_links_it() {
    let packages = repo::path_packages();
    let (examined, violations) = back_edges(&packages, repo::normal_closure);
    assert!(
        examined >= 10,
        "the dev edge scan lost its corpus: {examined}"
    );
    assert!(
        violations.is_empty(),
        "a package's normal closure reaches a crate that dev-depends on it; \
         that crate's tests would link it twice and its types would not \
         unify: {violations:?}"
    );
}

/// A package the graph says exists only for tests must say so itself: a
/// shipped binary must not be able to reach it unnoticed, and the arch checks
/// recognise a dev-only fixture by that self-classification.
#[test]
fn every_dev_only_package_classifies_itself_test_dev_tool() {
    let packages = repo::path_packages();
    let dev_only = dev_only_by_graph(&packages);
    let names: Vec<&str> = dev_only
        .iter()
        .map(|package| package["name"].as_str().expect("package name"))
        .collect();
    assert!(
        names.contains(&"zeroship-testkit"),
        "the dev-only scan lost its corpus: {names:?}"
    );
    let unclassified: BTreeSet<String> = dev_only
        .iter()
        .filter(|package| !repo::self_classifies_test_dev_tool(package))
        .map(|package| package["name"].as_str().expect("package name").to_owned())
        .collect();
    assert!(
        unclassified.is_empty(),
        "the graph says these packages exist only for tests, but their \
         manifests do not classify themselves `test-dev-tool`: {unclassified:?}"
    );
}

/// The controls for [`every_dev_only_package_classifies_itself_test_dev_tool`]:
/// an unclassified helper named only on dev edges is reported, a package named
/// on a normal edge from a shipped binary is not dev-only at all, and a
/// classified testkit on the same dev edges is not reported.
#[test]
fn the_dev_only_graph_reader_reports_unclassified_helpers_and_spares_reached_packages() {
    let helper = serde_json::json!({
        "name": "zeroship-helper",
        "targets": [{"name": "zeroship-helper", "kind": ["lib"]}],
        "dependencies": []
    });
    let helper_consumer = serde_json::json!({
        "name": "zeroship-helper-consumer",
        "targets": [{"name": "zeroship-helper-consumer", "kind": ["lib"]}],
        "dependencies": [{"name": "zeroship-helper", "kind": "dev"}]
    });
    let testkit = serde_json::json!({
        "name": "zeroship-testkit",
        "targets": [{"name": "zeroship-testkit", "kind": ["lib"]}],
        "dependencies": [],
        "metadata": {"zeroship-config": {"targets": [
            {"target": "zeroship-testkit", "class": "test-dev-tool"}
        ]}}
    });
    let testkit_consumer = serde_json::json!({
        "name": "zeroship-domain",
        "targets": [{"name": "zeroship-domain", "kind": ["lib"]}],
        "dependencies": [{"name": "zeroship-testkit", "kind": "dev"}]
    });
    let reached = serde_json::json!({
        "name": "zeroship-reached",
        "targets": [{"name": "zeroship-reached", "kind": ["lib"]}],
        "dependencies": []
    });
    let shipped = serde_json::json!({
        "name": "zeroship-shipped",
        "targets": [{"name": "zeroship-shipped", "kind": ["bin"]}],
        "dependencies": [{"name": "zeroship-reached", "kind": null}]
    });
    let packages = [
        &helper,
        &helper_consumer,
        &testkit,
        &testkit_consumer,
        &reached,
        &shipped,
    ];

    let dev_only = dev_only_by_graph(&packages);
    let names: Vec<&str> = dev_only
        .iter()
        .map(|package| package["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["zeroship-helper", "zeroship-testkit"]);

    let unclassified: BTreeSet<String> = dev_only
        .iter()
        .filter(|package| !repo::self_classifies_test_dev_tool(package))
        .map(|package| package["name"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(unclassified, BTreeSet::from(["zeroship-helper".to_owned()]));
}

#[test]
fn the_reader_sees_renamed_and_target_specific_edges() {
    let testkit = serde_json::json!({
        "name": "zeroship-testkit",
        "dependencies": [],
        "metadata": {"zeroship-config": {"targets": [
            {"target": "zeroship-testkit", "class": "test-dev-tool"}
        ]}}
    });
    // A consumer whose dev edge to the testkit is renamed and sits in a
    // target-specific table: the alias and the table are not the identity.
    let consumer = serde_json::json!({
        "name": "zeroship-domain",
        "dependencies": [{
            "name": "zeroship-testkit", "rename": "kit",
            "kind": "dev", "target": "cfg(unix)"
        }]
    });
    let bystander = serde_json::json!({
        "name": "zeroship-bystander",
        "dependencies": [{"name": "zeroship-testkit", "kind": null, "target": null}]
    });
    // An unclassified helper, with no `metadata.zeroship-config` block at all:
    // the scan examines it on its name alone, not on a self-classification.
    let helper = serde_json::json!({
        "name": "zeroship-helper",
        "dependencies": []
    });
    let helper_consumer = serde_json::json!({
        "name": "zeroship-helper-consumer",
        "dependencies": [{"name": "zeroship-helper", "kind": "dev", "target": null}]
    });
    let packages = [&testkit, &consumer, &bystander, &helper, &helper_consumer];

    // The rejection control: both the classified testkit's closure and the
    // unclassified helper's closure reach their own dev-dependent consumer.
    let (examined, violations) = back_edges(&packages, |name| match name {
        "zeroship-testkit" => {
            BTreeSet::from(["zeroship-testkit".to_owned(), "zeroship-domain".to_owned()])
        }
        "zeroship-helper" => BTreeSet::from([
            "zeroship-helper".to_owned(),
            "zeroship-helper-consumer".to_owned(),
        ]),
        _ => BTreeSet::new(),
    });
    assert_eq!(examined, 2, "only the two dev edges name a consumer");
    assert_eq!(
        violations,
        BTreeMap::from([
            ("zeroship-testkit".to_owned(), vec!["zeroship-domain".to_owned()]),
            (
                "zeroship-helper".to_owned(),
                vec!["zeroship-helper-consumer".to_owned()]
            ),
        ])
    );

    // The same two consumers, out of their package's own closure, pass.
    let (examined, violations) = back_edges(&packages, |name| BTreeSet::from([name.to_owned()]));
    assert_eq!(examined, 2);
    assert!(violations.is_empty());
}

/// The layer a dev-only library testkit's name places it in.
#[derive(Debug, PartialEq, Eq)]
enum TestkitLayer {
    /// `zeroship-testkit-server`, the bottom layer.
    Server,
    /// `zeroship-testkit`, the platform layer.
    Platform,
    /// `zeroship-<area>-testkit`, the area layers.
    Area,
}

/// The layer a testkit's name states, or `None` when the name is not a layer.
fn testkit_layer(package: &Value) -> Option<TestkitLayer> {
    let name = package["name"].as_str().expect("package name");
    if name == "zeroship-testkit-server" {
        Some(TestkitLayer::Server)
    } else if name == "zeroship-testkit" {
        Some(TestkitLayer::Platform)
    } else if name.starts_with("zeroship-") && name.ends_with("-testkit") {
        Some(TestkitLayer::Area)
    } else {
        None
    }
}

/// Whether a package's own manifest classifies it `test-dev-tool` and it has
/// no binary target. `zeroship-config-contract` is the tool that classifies
/// itself this way and still ships a binary, so the layer rule does not apply
/// to it.
fn is_layer_testkit(package: &Value) -> bool {
    repo::self_classifies_test_dev_tool(package) && repo::bin_targets(package).is_empty()
}

/// The path packages `package` names on a normal edge: `[dependencies]`, under
/// any alias or target table, filtered to the local packages `packages` holds.
fn normal_path_dependencies<'a>(packages: &[&'a Value], package: &Value) -> Vec<&'a Value> {
    package["dependencies"]
        .as_array()
        .expect("package dependencies")
        .iter()
        .filter(|dependency| dependency["kind"].is_null() && dependency["path"].is_string())
        .filter_map(|dependency| dependency["name"].as_str())
        .filter_map(|name| {
            packages
                .iter()
                .find(|candidate| candidate["name"] == name)
                .copied()
        })
        .collect()
}

/// Whether the package's manifest lives under the workspace's `libs/`
/// directory, where the standalone zero-dependency drivers live.
fn lives_under_libs(package: &Value) -> bool {
    package["manifest_path"]
        .as_str()
        .and_then(|manifest| std::path::Path::new(manifest).strip_prefix(repo::root()).ok())
        .is_some_and(|relative| relative.starts_with("libs"))
}

/// Every dev-only library testkit whose normal path edges reach outside the
/// layer its name states, keyed by the offending testkit.
///
/// The server layer is a leaf; the platform layer may name the server layer, a
/// `libs/` driver, or a crate with no path dependency of its own; an area
/// testkit may name the two layers below it and nothing else that is a testkit;
/// and a name that is none of those is not a testkit crate at all.
fn layer_violations(packages: &[&Value]) -> BTreeMap<String, String> {
    let mut violations = BTreeMap::new();
    for package in packages {
        if !is_layer_testkit(package) {
            continue;
        }
        let name = package["name"].as_str().expect("package name").to_owned();
        let dependencies = normal_path_dependencies(packages, package);
        let reason = match testkit_layer(package) {
            None => Some("its name does not place it in a testkit layer".to_owned()),
            Some(TestkitLayer::Server) => dependencies.first().map(|dependency| {
                format!(
                    "the server layer may have no path normal edge, but it names {}",
                    dependency["name"].as_str().expect("dependency name")
                )
            }),
            Some(TestkitLayer::Platform) => dependencies.iter().find_map(|dependency| {
                let dependency_name = dependency["name"].as_str().expect("dependency name");
                if testkit_layer(dependency) == Some(TestkitLayer::Area) {
                    return Some(format!(
                        "the platform layer may not name the area testkit {dependency_name}, \
                         which sits above it"
                    ));
                }
                let allowed = dependency_name == "zeroship-testkit-server"
                    || lives_under_libs(dependency)
                    || normal_path_dependencies(packages, dependency).is_empty();
                (!allowed).then(|| {
                    format!(
                        "the platform layer may not name {dependency_name}: it is neither \
                         the server layer, a `libs/` driver, nor a crate with no path dependency"
                    )
                })
            }),
            Some(TestkitLayer::Area) => dependencies.iter().find_map(|dependency| {
                let dependency = dependency["name"].as_str().expect("dependency name");
                (dependency.ends_with("-testkit") && dependency != "zeroship-testkit").then(|| {
                    format!("an area testkit may not name the testkit {dependency}")
                })
            }),
        };
        if let Some(reason) = reason {
            violations.insert(name, reason);
        }
    }
    violations
}

/// A minimal path package for the layer controls: `name`'s manifest sits under
/// `directory`, it is a library only, and each entry in `dependencies` is one
/// normal path edge to a package of that name.
fn synthetic_path_package(name: &str, directory: &str, dependencies: &[&str]) -> Value {
    serde_json::json!({
        "name": name,
        "manifest_path": repo::root().join(directory).join("Cargo.toml").to_str().unwrap(),
        "targets": [{"name": name, "kind": ["lib"]}],
        "dependencies": dependencies
            .iter()
            .map(|dependency| serde_json::json!({
                "name": dependency,
                "kind": null,
                "path": repo::root().join("crates").join(dependency).to_str().unwrap(),
            }))
            .collect::<Vec<_>>(),
    })
}

/// [`synthetic_path_package`] with the `test-dev-tool` self-classification the
/// layer reader selects on, under `crates/`.
fn synthetic_testkit(name: &str, dependencies: &[&str]) -> Value {
    let mut package = synthetic_path_package(name, &format!("crates/{name}"), dependencies);
    package["metadata"] = serde_json::json!({
        "zeroship-config": {"targets": [{"target": name, "class": "test-dev-tool"}]}
    });
    package
}

/// Every dev-only library testkit sits in the layer its name states.
#[test]
fn test_support_crates_sit_in_the_layer_their_name_says() {
    let packages = repo::path_packages();
    let layer_testkits: Vec<&str> = packages
        .iter()
        .filter(|package| is_layer_testkit(package))
        .map(|package| package["name"].as_str().expect("package name"))
        .collect();
    assert!(
        layer_testkits.contains(&"zeroship-testkit-server")
            && layer_testkits.contains(&"zeroship-testkit")
            && layer_testkits
                .iter()
                .any(|name| name.ends_with("-testkit") && *name != "zeroship-testkit"),
        "the layer scan lost its corpus: {layer_testkits:?}"
    );
    let violations = layer_violations(&packages);
    assert!(
        violations.is_empty(),
        "a dev-only testkit's normal edges reach outside the layer its name places \
         it in: {violations:?}"
    );
}

/// The controls for [`test_support_crates_sit_in_the_layer_their_name_says`]:
/// one misplaced normal edge per layer, a name that is not a testkit layer at
/// all, and a well-formed set the same reader accepts.
#[test]
fn the_layer_reader_rejects_misplaced_edges_and_names_that_are_not_layers() {
    let server = synthetic_testkit("zeroship-testkit-server", &["zeroship-stray"]);
    let stray = synthetic_path_package("zeroship-stray", "crates/zeroship-stray", &[]);
    assert_eq!(
        layer_violations(&[&server, &stray]),
        BTreeMap::from([(
            "zeroship-testkit-server".to_owned(),
            "the server layer may have no path normal edge, but it names zeroship-stray"
                .to_owned(),
        )])
    );

    let platform = synthetic_testkit("zeroship-testkit", &["zeroship-core"]);
    let core = synthetic_path_package("zeroship-core", "crates/zeroship-core", &["zeroship-leaf"]);
    let leaf = synthetic_path_package("zeroship-leaf", "crates/zeroship-leaf", &[]);
    assert_eq!(
        layer_violations(&[&platform, &core, &leaf]),
        BTreeMap::from([(
            "zeroship-testkit".to_owned(),
            "the platform layer may not name zeroship-core: it is neither the server \
             layer, a `libs/` driver, nor a crate with no path dependency"
                .to_owned(),
        )])
    );

    // An area testkit with no path dependency of its own is still above the
    // platform layer, so the leaf allowance does not admit it.
    let platform = synthetic_testkit("zeroship-testkit", &["zeroship-data-testkit"]);
    let leaf_area = synthetic_testkit("zeroship-data-testkit", &[]);
    assert_eq!(
        layer_violations(&[&platform, &leaf_area]),
        BTreeMap::from([(
            "zeroship-testkit".to_owned(),
            "the platform layer may not name the area testkit zeroship-data-testkit, \
             which sits above it"
                .to_owned(),
        )])
    );

    let area = synthetic_testkit("zeroship-data-testkit", &["zeroship-migrate-testkit"]);
    let sibling = synthetic_testkit("zeroship-migrate-testkit", &[]);
    assert_eq!(
        layer_violations(&[&area, &sibling]),
        BTreeMap::from([(
            "zeroship-data-testkit".to_owned(),
            "an area testkit may not name the testkit zeroship-migrate-testkit".to_owned(),
        )])
    );

    let fixtures = synthetic_testkit("zeroship-workflow-fixtures", &[]);
    assert_eq!(
        layer_violations(&[&fixtures]),
        BTreeMap::from([(
            "zeroship-workflow-fixtures".to_owned(),
            "its name does not place it in a testkit layer".to_owned(),
        )])
    );

    // The structure the layers describe passes: the server is a leaf, the
    // platform names the server, a `libs/` driver and a crate with no path
    // dependency of its own, and an area testkit names only the platform.
    let server = synthetic_testkit("zeroship-testkit-server", &[]);
    let driver = synthetic_path_package("compio-postgres", "libs/compio-postgres", &[]);
    let leaf = synthetic_path_package("zeroship-id", "crates/zeroship-id", &[]);
    let platform = synthetic_testkit(
        "zeroship-testkit",
        &["zeroship-testkit-server", "compio-postgres", "zeroship-id"],
    );
    let area = synthetic_testkit("zeroship-data-testkit", &["zeroship-testkit"]);
    assert!(layer_violations(&[&server, &driver, &leaf, &platform, &area]).is_empty());
}
