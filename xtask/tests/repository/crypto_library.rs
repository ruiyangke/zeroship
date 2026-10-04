//! aws-lc-rs is the one cryptography library a build of this workspace
//! compiles: rustls, jsonwebtoken and rcgen each select it by feature.
//!
//! `ring` implements the same primitives a second time, and its build script
//! declares `rerun-if-env-changed` on `CARGO_MANIFEST_DIR` and the
//! `CARGO_PKG_*` variables. A cargo started from a test inherits the test
//! crate's values for those (`zeroship_testkit::nested_cargo` says why), and
//! every crate's trybuild project builds in one shared target directory. With
//! `ring` in the graph, each compile-fail suite that follows another crate's
//! re-runs that build script and rebuilds every crate above it.
//!
//! Update these and the root Cargo.toml crypto comment together.

use super::write_fixture_workspace;
use crate::architecture::repo;
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::process::Command;

const FORBIDDEN: &str = "ring";
const SELECTED: &str = "aws-lc-rs";

/// A compiled dependency graph as `cargo tree --workspace` prints it.
#[derive(Debug)]
struct Graph {
    /// The packages printed at depth zero: every member, under `--workspace`.
    roots: BTreeSet<String>,
    /// Each package printed anywhere, with the packages printed one level above
    /// it.
    dependents: BTreeMap<String, BTreeSet<String>>,
}

/// Every dependency edge some build of the workspace compiles: Cargo's own
/// feature resolution over normal, build and dev edges, with every member
/// feature, on every target platform.
///
/// Cargo selects the activated edges. The resolve `cargo metadata` reports
/// cannot: like Cargo.lock it also keeps an optional dependency that a weak
/// `dependency?/feature` names without enabling, which is how rustls-webpki's
/// `alloc = ["ring?/alloc"]` leaves a `ring` package in Cargo.lock that no
/// build compiles.
fn compiled_graph(root: &Path) -> Result<Graph, String> {
    let output = Command::new(env!("CARGO"))
        .current_dir(root)
        .args([
            "tree",
            "--locked",
            "--workspace",
            "--all-features",
            "--target",
            "all",
            "-e",
            "normal,build,dev",
            "--prefix",
            "depth",
            "--format",
            "{p}",
            "--color",
            "never",
        ])
        .output()
        .map_err(|error| error.to_string())?;
    if !output.status.success() {
        return Err(format!(
            "cargo tree failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    read_graph(&String::from_utf8(output.stdout).map_err(|error| error.to_string())?)
}

/// The graph in a `cargo tree --prefix depth --format {p}` output, where a
/// row's dependent is the nearest row above it one level up.
///
/// Every row must parse and no row may skip a level, so a format this reader
/// does not know fails instead of reading as an empty graph. Output with no
/// rows is refused: every tree prints its roots.
fn read_graph(output: &str) -> Result<Graph, String> {
    let row = regex::Regex::new(r"^([0-9]+)([A-Za-z0-9_-]+) v[0-9][^ ]*(?: \(.+\))?$").unwrap();
    let mut path: Vec<String> = Vec::new();
    let mut graph = Graph {
        roots: BTreeSet::new(),
        dependents: BTreeMap::new(),
    };
    // `--workspace` separates the members' trees with an empty line.
    for line in output.lines().filter(|line| !line.is_empty()) {
        let line = line.strip_suffix(" (*)").unwrap_or(line);
        let capture = row
            .captures(line)
            .ok_or_else(|| format!("unexpected cargo tree row: {line:?}"))?;
        let depth: usize = capture[1]
            .parse()
            .map_err(|_| format!("row depth: {line:?}"))?;
        if depth > path.len() {
            return Err(format!("row skips a level: {line:?}"));
        }
        path.truncate(depth);
        let name = capture[2].to_owned();
        let entry = graph.dependents.entry(name.clone()).or_default();
        match path.last() {
            Some(parent) => {
                entry.insert(parent.clone());
            }
            None => {
                graph.roots.insert(name.clone());
            }
        }
        path.push(name);
    }
    if graph.roots.is_empty() {
        return Err("empty cargo tree output".into());
    }
    Ok(graph)
}

/// No build of the workspace compiles `ring`, on any edge kind, with any
/// member feature, for any target platform.
///
/// The absence means something only because the reader saw the graph: every
/// workspace member is one of its roots, and the library the workspace does
/// compile is in it with the packages that depend on it.
#[test]
fn no_workspace_build_compiles_ring() {
    let graph = compiled_graph(&repo::root()).unwrap();
    let members: BTreeSet<String> = repo::workspace()
        .iter()
        .map(|package| package["name"].as_str().expect("package name").to_owned())
        .collect();
    assert!(members.len() >= 20, "workspace scan lost its packages");
    assert_eq!(
        graph.roots, members,
        "cargo tree's roots are not the workspace members"
    );
    assert!(
        graph
            .dependents
            .get(SELECTED)
            .is_some_and(|parents| !parents.is_empty()),
        "the reader does not see `{SELECTED}` depended on, so it cannot vouch for an absence"
    );
    if let Some(parents) = graph.dependents.get(FORBIDDEN) {
        panic!(
            "a workspace build compiles `{FORBIDDEN}`, depended on by {parents:?}; select \
             `{SELECTED}` on that edge and update the root Cargo.toml crypto comment"
        );
    }
}

#[test]
fn tree_reader_takes_roots_and_dependents_from_depth_and_refuses_what_it_cannot_read() {
    let output = "\
0app v0.1.0 (/a path)\n\
1signer v0.1.0 (/a path)\n\
2ring v0.17.14\n\
1macros v0.1.0 (proc-macro)\n\
2ring v0.17.14 (*)\n\
\n\
0svc v0.1.0 (/a path)\n\
1ring v0.17.14 (*)\n\
1app v0.1.0 (/a path) (*)\n";
    let graph = read_graph(output).unwrap();
    assert_eq!(graph.roots, BTreeSet::from(["app".into(), "svc".into()]));
    assert_eq!(
        graph.dependents["ring"],
        BTreeSet::from(["signer".into(), "macros".into(), "svc".into()])
    );
    // A member printed below another is still a root of its own tree.
    assert_eq!(graph.dependents["app"], BTreeSet::from(["svc".into()]));
    assert_eq!(graph.dependents["signer"], BTreeSet::from(["app".into()]));
    assert!(!graph.roots.contains("signer"));
    for refused in [
        "",
        "\n",
        "0app v0.1.0\n2ring v0.17.14\n",
        "0app v0.1.0\n1[dev-dependencies]\n",
        "app v0.1.0\n",
    ] {
        assert!(read_graph(refused).is_err(), "accepted {refused:?}");
    }
}

/// The packages Cargo.lock records `package` as depending on.
fn locked_dependencies(lockfile: &str, package: &str) -> BTreeSet<String> {
    let lock = lockfile.parse::<toml_edit::DocumentMut>().unwrap();
    let packages = lock["package"]
        .as_array_of_tables()
        .expect("lockfile packages");
    let entry = packages
        .iter()
        .find(|entry| entry["name"].as_str() == Some(package))
        .unwrap_or_else(|| panic!("Cargo.lock has no {package}"));
    entry
        .get("dependencies")
        .and_then(toml_edit::Item::as_array)
        .map(|dependencies| {
            dependencies
                .iter()
                .map(|dependency| dependency.as_str().expect("dependency").to_owned())
                .collect()
        })
        .unwrap_or_default()
}

/// The reader over a real Cargo resolve: a dev edge, a build edge declared for
/// another platform and an optional dependency only a member feature turns on
/// each compile `ring` and are each named; an optional dependency that only a
/// weak feature names compiles nothing and is not, though Cargo.lock records it.
#[test]
fn cargo_resolve_names_every_compiled_ring_edge_and_no_weak_one() {
    let fixture = tempfile::tempdir().unwrap();
    let root = fixture.path();
    write_fixture_workspace(
        root,
        &["app", "desk", "tool", "svc"],
        &[
            ("ring", "[features]\nalloc=[]\n", &["src/lib.rs"]),
            (
                "signer",
                "[dependencies]\nring={path='../ring', optional=true}\n\
                 [features]\ncrypto=['dep:ring']\n",
                &["src/lib.rs"],
            ),
            (
                "carrier",
                "[dependencies]\nring={path='../ring', optional=true}\n\
                 [features]\ndefault=['alloc']\nalloc=['ring?/alloc']\n",
                &["src/lib.rs"],
            ),
            (
                "app",
                "[dev-dependencies]\nring={path='../ring'}\n",
                &["src/main.rs"],
            ),
            (
                "desk",
                "[target.'cfg(target_os = \"macos\")'.build-dependencies]\nring={path='../ring'}\n",
                &["src/main.rs", "build.rs"],
            ),
            (
                "tool",
                "[dependencies]\nsigner={path='../signer'}\n[features]\nsign=['signer/crypto']\n",
                &["src/main.rs"],
            ),
            (
                "svc",
                "[dependencies]\ncarrier={path='../carrier'}\n",
                &["src/main.rs"],
            ),
        ],
    );

    let graph = compiled_graph(root).unwrap();
    assert_eq!(
        graph.roots,
        BTreeSet::from(["app".into(), "desk".into(), "tool".into(), "svc".into()])
    );
    assert_eq!(
        graph.dependents.get(FORBIDDEN),
        Some(&BTreeSet::from([
            "app".into(),
            "desk".into(),
            "signer".into()
        ])),
        "{graph:?}"
    );
    // The weak carrier is compiled; its `ring` edge is not, though the lock
    // resolve records it.
    assert_eq!(graph.dependents["carrier"], BTreeSet::from(["svc".into()]));
    let lockfile = std::fs::read_to_string(root.join("Cargo.lock")).unwrap();
    assert!(
        locked_dependencies(&lockfile, "carrier").contains(FORBIDDEN),
        "Cargo.lock does not record the weak edge, so this case does not show \
         why the reader asks Cargo's feature resolution"
    );

    std::fs::remove_file(root.join("Cargo.lock")).unwrap();
    assert!(
        compiled_graph(root).is_err(),
        "a resolve without its lockfile was accepted"
    );
}
