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

use crate::architecture::repo;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

/// Every local package `cargo metadata` resolves: no registry or git source,
/// whether or not the root manifest lists it as a workspace member.
fn path_packages() -> Vec<&'static Value> {
    repo::metadata()["packages"]
        .as_array()
        .expect("packages")
        .iter()
        .filter(|package| package["source"].is_null())
        .collect()
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
    let packages = path_packages();
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
