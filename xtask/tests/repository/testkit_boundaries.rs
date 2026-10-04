//! A dev-only package must not depend on a crate whose own tests dev-depend on
//! it: cargo would then build that crate twice when its tests compile, and the
//! two copies' types would not unify. A domain type named inside a testkit is
//! what would pull such a normal edge back.
//!
//! The rule is read from `cargo metadata`, not from manifest text: a dependency
//! is identified by its real package name whatever alias a `package =` rename
//! gives it, and a target-specific table counts like any other normal edge.

use crate::architecture::repo;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

/// Whether the manifest classifies the package itself `test-dev-tool`.
fn self_classified_test_dev_tool(package: &Value) -> bool {
    let name = package["name"].as_str().expect("package name");
    package["metadata"]["zeroship-config"]["targets"]
        .as_array()
        .is_some_and(|targets| {
            targets
                .iter()
                .any(|target| target["target"] == name && target["class"] == "test-dev-tool")
        })
}

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

/// Each dev-only package's consumers that its own normal closure reaches.
fn back_edges(
    workspace: &[&Value],
    closure: impl Fn(&str) -> BTreeSet<String>,
) -> (usize, BTreeMap<String, Vec<String>>) {
    let mut examined = 0;
    let mut violations = BTreeMap::new();
    for testkit in workspace.iter().filter(|package| self_classified_test_dev_tool(package)) {
        let name = testkit["name"].as_str().unwrap();
        let reached = closure(name);
        let consumers: Vec<String> = workspace
            .iter()
            .filter(|package| dev_dependencies(package).contains(name))
            .map(|package| package["name"].as_str().unwrap().to_owned())
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
fn no_dev_only_package_depends_on_a_crate_whose_tests_use_it() {
    let workspace = repo::workspace();
    let (examined, violations) = back_edges(&workspace, repo::normal_closure);
    assert!(
        examined >= 10,
        "the dev-only consumer scan lost its corpus: {examined}"
    );
    assert!(
        violations.is_empty(),
        "a dev-only package's normal closure reaches a crate whose tests \
         dev-depend on it; those tests would link that crate twice and its \
         types would not unify: {violations:?}"
    );
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
    let workspace = [&testkit, &consumer, &bystander];

    // The rejection control: the testkit's closure reaches its consumer.
    let (examined, violations) = back_edges(&workspace, |_| {
        BTreeSet::from(["zeroship-testkit".to_owned(), "zeroship-domain".to_owned()])
    });
    assert_eq!(examined, 1, "only the dev edge names a consumer");
    assert_eq!(
        violations,
        BTreeMap::from([(
            "zeroship-testkit".to_owned(),
            vec!["zeroship-domain".to_owned()]
        )])
    );

    // The same consumer, out of the testkit's closure, passes.
    let (examined, violations) =
        back_edges(&workspace, |_| BTreeSet::from(["zeroship-testkit".to_owned()]));
    assert_eq!(examined, 1);
    assert!(violations.is_empty());
}
