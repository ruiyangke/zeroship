use super::repo;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

/// The dependency facts `tests/testkit` must share with the `[workspace.dependencies]`
/// entry it cannot inherit from, because it is an excluded crate and therefore
/// cannot write `workspace = true`.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Requirement {
    version: Option<String>,
    features: BTreeSet<String>,
    default_features: bool,
    path: Option<PathBuf>,
}

fn requirement(item: &toml_edit::Item, base: &Path) -> Requirement {
    if let Some(version) = item.as_str() {
        return Requirement {
            version: Some(version.to_owned()),
            features: BTreeSet::new(),
            default_features: true,
            path: None,
        };
    }
    let table = item
        .as_table_like()
        .expect("a dependency is a version string or a table");
    let string = |key: &str| table.get(key).and_then(toml_edit::Item::as_str).map(str::to_owned);
    let features = table
        .get("features")
        .and_then(toml_edit::Item::as_array)
        .map(|array| {
            array
                .iter()
                .filter_map(toml_edit::Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();
    Requirement {
        version: string("version"),
        features,
        default_features: table
            .get("default-features")
            .and_then(toml_edit::Item::as_bool)
            .unwrap_or(true),
        path: string("path").map(|path| {
            let joined = base.join(&path);
            joined.canonicalize().unwrap_or(joined)
        }),
    }
}

fn dependencies(source: &str, table: &str, base: &Path) -> BTreeMap<String, Requirement> {
    let manifest = source
        .parse::<toml_edit::DocumentMut>()
        .unwrap_or_else(|error| panic!("parse manifest: {error}"));
    let table_like = match table {
        "workspace.dependencies" => manifest
            .get("workspace")
            .and_then(toml_edit::Item::as_table_like)
            .and_then(|workspace| workspace.get("dependencies"))
            .and_then(toml_edit::Item::as_table_like),
        _ => manifest
            .get("dependencies")
            .and_then(toml_edit::Item::as_table_like),
    }
    .unwrap_or_else(|| panic!("missing {table} table"));
    table_like
        .iter()
        .map(|(alias, item)| (alias.to_owned(), requirement(item, base)))
        .collect()
}

/// Compare every dependency the two tables share; return how many were examined
/// and the aliases whose facts differ.
fn mismatches(
    shared: &BTreeMap<String, Requirement>,
    workspace: &BTreeMap<String, Requirement>,
) -> (usize, Vec<String>) {
    let mut examined = 0;
    let mut violations = Vec::new();
    for (alias, testkit) in shared {
        let Some(workspace) = workspace.get(alias) else {
            continue;
        };
        examined += 1;
        if testkit != workspace {
            violations.push(format!(
                "{alias}: testkit {testkit:?} does not match workspace {workspace:?}"
            ));
        }
    }
    (examined, violations)
}

/// Assert every dependency a manifest shares with `[workspace.dependencies]`
/// keeps that entry's version requirement, features, default-features and path,
/// and that the scan examined at least `minimum` shared entries.
fn assert_manifest_pins(directory: &str, minimum: usize) {
    let root = repo::root();
    let dir = root.join(directory);
    let workspace = dependencies(&repo::read("Cargo.toml"), "workspace.dependencies", &root);
    let source = std::fs::read_to_string(dir.join("Cargo.toml"))
        .unwrap_or_else(|error| panic!("read {directory}/Cargo.toml: {error}"));
    let shared = dependencies(&source, "dependencies", &dir);

    let (examined, violations) = mismatches(&shared, &workspace);
    assert!(
        examined >= minimum,
        "the shared-dependency scan for {directory} lost its entries: {examined}"
    );
    assert!(
        violations.is_empty(),
        "{directory} must keep every workspace-shared dependency's version \
         requirement, features, default-features and path; change the workspace \
         entry or the manifest entry, never one of them:\n{}",
        violations.join("\n")
    );
}

#[test]
fn testkit_dependencies_match_the_workspace_requirements() {
    assert_manifest_pins("tests/testkit", 8);
}

#[test]
fn data_testkit_dependencies_match_the_workspace_requirements() {
    assert_manifest_pins("crates/zeroship-data-testkit", 8);
}

/// The data testkit speaks plain data and leaf crates, never a data-plane
/// domain crate.
///
/// Those crates' own unit tests reach the testkit as an ordinary
/// `[dev-dependencies]` entry, so a normal edge back would make cargo build the
/// crate twice when its unit tests compile and the two copies' types would not
/// unify. The dependency closure is the thing that has to stay clean; a domain
/// type named inside the testkit is what would pull the edge back.
#[test]
fn data_testkit_depends_on_no_data_plane_crate() {
    let root = repo::root();
    let dir = root.join("crates/zeroship-data-testkit");
    let source = std::fs::read_to_string(dir.join("Cargo.toml"))
        .expect("read crates/zeroship-data-testkit/Cargo.toml");
    let declared = dependencies(&source, "dependencies", &dir);
    let forbidden = declared
        .keys()
        .filter(|name| name.starts_with("zeroship-data-"))
        .collect::<Vec<_>>();
    assert!(
        forbidden.is_empty(),
        "the data testkit must not depend on a data-plane domain crate; a crate \
         whose unit tests dev-depend on the testkit cannot also be on its normal \
         edge, or the test build links two copies of it: {forbidden:?}"
    );
}

#[test]
fn the_comparison_reports_a_changed_version_or_feature_set() {
    let base = Requirement {
        version: Some("1".to_owned()),
        features: BTreeSet::new(),
        default_features: true,
        path: None,
    };
    let shared = BTreeMap::from([("compio".to_owned(), base.clone())]);
    assert_eq!(
        mismatches(&shared, &BTreeMap::from([("compio".to_owned(), base.clone())])),
        (1, Vec::new())
    );

    let bumped = Requirement {
        version: Some("2".to_owned()),
        ..base.clone()
    };
    assert_eq!(
        mismatches(&shared, &BTreeMap::from([("compio".to_owned(), bumped)])).1.len(),
        1
    );

    let featured = Requirement {
        features: BTreeSet::from(["runtime".to_owned()]),
        ..base.clone()
    };
    assert_eq!(
        mismatches(&shared, &BTreeMap::from([("compio".to_owned(), featured)])).1.len(),
        1
    );

    let default_off = Requirement {
        default_features: false,
        ..base.clone()
    };
    assert_eq!(
        mismatches(&shared, &BTreeMap::from([("compio".to_owned(), default_off)])).1.len(),
        1
    );

    // A dependency the workspace does not declare as well is not examined.
    assert_eq!(
        mismatches(
            &BTreeMap::from([("testcontainers".to_owned(), base)]),
            &BTreeMap::new()
        ),
        (0, Vec::new())
    );
}
