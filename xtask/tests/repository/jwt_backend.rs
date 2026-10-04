//! jsonwebtoken takes its crypto backend from its own features, and only when
//! exactly one of `aws_lc_rs` and `rust_crypto` is enabled. With both or with
//! neither, `CryptoProvider::from_crate_features` returns a provider whose every
//! sign and verify panics. Features unify per cargo invocation, so a dependency
//! of one package can arm that panic in every binary the same build produces -
//! the deployed image is one `cargo build` over several packages.
//!
//! So every build resolves jsonwebtoken with one feature set, and that set
//! selects aws-lc-rs, the workspace's one crypto library (`crypto_library.rs`).
//!
//! Update these and the root Cargo.toml `jsonwebtoken` entry together.

use super::tls_provider::{feature_tree, package_features, process_packages, Resolve, Scope};
use super::tokio_boundary::dockerfile_build;
use super::write_fixture_workspace;
use crate::architecture::repo;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

const PACKAGE: &str = "jsonwebtoken";
const BACKENDS: &[&str] = &["aws_lc_rs", "rust_crypto"];
const SELECTED: &str = "aws_lc_rs";
/// The feature set every build resolves jsonwebtoken with.
const RESOLVED: &[&str] = &["aws_lc_rs"];

/// Why `features` would not give jsonwebtoken [`SELECTED`] as its one backend,
/// if they would not.
fn backend_violation(features: &BTreeSet<String>) -> Option<String> {
    let enabled: Vec<&str> = BACKENDS
        .iter()
        .copied()
        .filter(|backend| features.contains(*backend))
        .collect();
    match enabled.as_slice() {
        [backend] if *backend == SELECTED => None,
        [] => Some(format!(
            "no backend in {features:?}, so every sign and verify panics"
        )),
        [backend] => Some(format!(
            "backend `{backend}` in {features:?}; `{SELECTED}` selects aws-lc-rs, the \
             workspace's one crypto library"
        )),
        _ => Some(format!(
            "backends {enabled:?} in {features:?}, so every sign and verify panics"
        )),
    }
}

/// What is wrong with how these builds resolve jsonwebtoken: each build whose
/// features do not select [`SELECTED`] alone, and each whose feature set is not
/// [`RESOLVED`]. No builds at all is a finding, so a scan that read nothing
/// cannot pass.
fn findings(builds: &BTreeMap<String, BTreeSet<String>>) -> Vec<String> {
    if builds.is_empty() {
        return vec!["no build resolves jsonwebtoken".into()];
    }
    let expected: BTreeSet<String> = RESOLVED
        .iter()
        .map(|feature| (*feature).to_owned())
        .collect();
    builds
        .iter()
        .filter_map(|(build, features)| {
            backend_violation(features)
                .or_else(|| {
                    (*features != expected)
                        .then(|| format!("features {features:?}, not {expected:?}"))
                })
                .map(|finding| format!("{build}: {finding}"))
        })
        .collect()
}

/// The jsonwebtoken features one resolve enables, or `None` when it does not
/// link jsonwebtoken.
fn resolved(
    root: &Path,
    scope: Scope<'_>,
    resolve: Resolve,
) -> Result<Option<BTreeSet<String>>, String> {
    package_features(&feature_tree(root, scope, resolve)?, PACKAGE)
}

/// Every build that produces a binary or runs a test resolves jsonwebtoken to
/// aws-lc-rs alone, with the same features: the workspace with and without dev
/// edges and member features on every target, the deployed image's one
/// `cargo build` over the Dockerfile's package selection, and each process
/// package built on its own, where a package that reaches jsonwebtoken only
/// through a dependency naming no backend would resolve none.
#[test]
fn every_build_resolves_jsonwebtoken_to_aws_lc_rs_alone() {
    let root = repo::root();
    let mut builds = BTreeMap::new();
    for include_dev in [false, true] {
        for all_features in [false, true] {
            let resolve = Resolve {
                include_dev,
                all_features,
                all_targets: true,
            };
            let features = resolved(&root, Scope::Workspace, resolve)
                .unwrap()
                .expect("the workspace links jsonwebtoken");
            builds.insert(format!("the workspace ({resolve:?})"), features);
        }
    }

    let image = dockerfile_build(&repo::read("deploy/Dockerfile")).unwrap();
    assert!(
        image.len() >= 5,
        "the Dockerfile build selection lost its packages: {image:?}"
    );
    let selection: Vec<&str> = image.iter().map(String::as_str).collect();
    let features = resolved(&root, Scope::Packages(&selection), Resolve::SHIPPED_ON_HOST)
        .unwrap()
        .expect("the deployed image links jsonwebtoken");
    builds.insert(format!("the Dockerfile build {selection:?}"), features);

    let processes = process_packages(&repo::workspace());
    assert!(processes.len() >= 5, "binary scan lost its packages");
    let mut linking = 0;
    for name in processes {
        if let Some(features) =
            resolved(&root, Scope::Packages(&[name]), Resolve::SHIPPED_ON_HOST).unwrap()
        {
            linking += 1;
            builds.insert(format!("{name} built alone"), features);
        }
    }
    assert!(
        linking >= 5,
        "jsonwebtoken scan lost the binaries that link it"
    );

    let findings = findings(&builds);
    assert!(
        findings.is_empty(),
        "builds that do not resolve jsonwebtoken to `{SELECTED}` alone: {findings:?}; \
         `cargo tree -e features -i jsonwebtoken` with the same selection names the edge"
    );
}

#[test]
fn backend_check_takes_aws_lc_rs_alone_and_refuses_both_neither_and_the_other() {
    let set = |features: &[&str]| -> BTreeSet<String> {
        features
            .iter()
            .map(|feature| (*feature).to_owned())
            .collect()
    };
    assert_eq!(backend_violation(&set(&["aws_lc_rs"])), None);
    assert_eq!(backend_violation(&set(&["aws_lc_rs", "use_pem"])), None);
    for (rejected, naming) in [
        (set(&[]), "no backend"),
        (set(&["use_pem"]), "no backend"),
        (set(&["aws_lc_rs", "rust_crypto"]), "backends"),
        (set(&["rust_crypto"]), "`rust_crypto`"),
    ] {
        let violation =
            backend_violation(&rejected).unwrap_or_else(|| panic!("accepted {rejected:?}"));
        assert!(violation.contains(naming), "{rejected:?}: {violation}");
    }

    let builds = |features: &[&str]| BTreeMap::from([("a build".to_owned(), set(features))]);
    assert!(findings(&builds(RESOLVED)).is_empty());
    assert_eq!(findings(&BTreeMap::new()).len(), 1);
    // A second feature beside the backend is still one backend, and still not
    // the set every build resolves.
    assert_eq!(findings(&builds(&["aws_lc_rs", "use_pem"])).len(), 1);
    assert_eq!(findings(&builds(&["rust_crypto"])).len(), 1);
}

/// The checks over a real Cargo resolve. Two packages that each build alone
/// with one backend resolve both when one invocation selects them together, as
/// the Dockerfile's does; a package that reaches jsonwebtoken only through a
/// dependency naming no backend resolves neither; a dev edge adds a backend
/// only where dev edges are admitted; a package that never links jsonwebtoken
/// is told apart from one that links it with no features.
#[test]
fn cargo_resolve_shows_both_backends_by_selection_and_neither_by_carrier() {
    let fixture = tempfile::tempdir().unwrap();
    let root = fixture.path();
    let backend = |feature: &str| {
        format!(
            "jsonwebtoken={{path='../jsonwebtoken', default-features=false, \
             features=['{feature}']}}\n"
        )
    };
    write_fixture_workspace(
        root,
        &["gateway", "relay", "bare", "tested", "plain"],
        &[
            (
                "jsonwebtoken",
                "[features]\ndefault=['use_pem']\nuse_pem=[]\naws_lc_rs=[]\nrust_crypto=[]\n",
                &["src/lib.rs"],
            ),
            (
                "carrier",
                "[dependencies]\njsonwebtoken={path='../jsonwebtoken', default-features=false}\n",
                &["src/lib.rs"],
            ),
            (
                "gateway",
                &format!("[dependencies]\n{}", backend("aws_lc_rs")),
                &["src/main.rs"],
            ),
            (
                "relay",
                &format!("[dependencies]\n{}", backend("rust_crypto")),
                &["src/main.rs"],
            ),
            (
                "bare",
                "[dependencies]\ncarrier={path='../carrier'}\n",
                &["src/main.rs"],
            ),
            (
                "tested",
                &format!(
                    "[dependencies]\n{}[dev-dependencies]\n{}",
                    backend("aws_lc_rs"),
                    backend("rust_crypto")
                ),
                &["src/main.rs"],
            ),
            ("plain", "", &["src/main.rs"]),
        ],
    );
    let shipped = |packages: &[&str]| {
        resolved(root, Scope::Packages(packages), Resolve::SHIPPED_ON_HOST)
            .unwrap()
            .unwrap_or_else(|| panic!("{packages:?} link jsonwebtoken"))
    };
    let violation = |features: &BTreeSet<String>| {
        backend_violation(features).unwrap_or_else(|| panic!("accepted {features:?}"))
    };

    assert_eq!(backend_violation(&shipped(&["gateway"])), None);
    // Alone, the relay has exactly one backend and would not panic, though it is
    // not the one this workspace selects.
    let relay = shipped(&["relay"]);
    assert_eq!(relay, BTreeSet::from(["rust_crypto".into()]));
    assert!(violation(&relay).contains("`rust_crypto`"), "{relay:?}");
    // Selected beside the gateway, as one image build selects them, it arms
    // the panic in both.
    let image = dockerfile_build(
        "FROM rust AS builder\nRUN cargo build --release \\\n    -p gateway \\\n    -p relay\n",
    )
    .unwrap();
    let selection: Vec<&str> = image.iter().map(String::as_str).collect();
    let together = shipped(&selection);
    assert!(violation(&together).contains("backends"), "{together:?}");

    let bare = shipped(&["bare"]);
    assert!(violation(&bare).contains("no backend"), "{bare:?}");

    assert_eq!(backend_violation(&shipped(&["tested"])), None);
    let with_dev = Resolve {
        include_dev: true,
        ..Resolve::SHIPPED_ON_HOST
    };
    let tested = resolved(root, Scope::Packages(&["tested"]), with_dev)
        .unwrap()
        .expect("tested links jsonwebtoken");
    assert!(violation(&tested).contains("backends"), "{tested:?}");

    let workspace = resolved(root, Scope::Workspace, Resolve::SHIPPED_ON_HOST)
        .unwrap()
        .expect("the fixture links jsonwebtoken");
    assert!(violation(&workspace).contains("backends"), "{workspace:?}");

    assert_eq!(
        resolved(root, Scope::Packages(&["plain"]), Resolve::SHIPPED_ON_HOST).unwrap(),
        None
    );
    assert!(
        resolved(root, Scope::Packages(&[]), Resolve::SHIPPED_ON_HOST).is_err(),
        "an empty selection resolved"
    );
}
