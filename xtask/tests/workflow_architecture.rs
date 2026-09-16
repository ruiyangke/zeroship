use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::process::Command;

/// The journal's schema crate must reach NOTHING else in the workspace.
///
/// That is the whole reason it exists. The migration service and the workflow
/// manager both depend on it; if it could reach the creator workflow engine,
/// depending on the artifacts would drag the engine into a platform service -
/// which is the coupling the schema-bundle path removed, and which
/// `workflow_process_dependencies_follow_crate_ownership` above forbids.
///
/// An `include_str!` reaching out of the crate directory is what this replaced,
/// and Cargo could not see that. It can see this.
#[test]
fn the_workflow_schema_crate_reaches_nothing_else_in_the_workspace() {
    let metadata = workspace_metadata();
    let members: BTreeSet<&str> = metadata["workspace_members"]
        .as_array()
        .unwrap()
        .iter()
        .map(|id| id.as_str().unwrap())
        .collect();
    let packages: BTreeMap<&str, &str> = metadata["packages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|package| {
            (
                package["id"].as_str().unwrap(),
                package["name"].as_str().unwrap(),
            )
        })
        .collect();
    let nodes: BTreeMap<&str, &serde_json::Value> = metadata["resolve"]["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|node| (node["id"].as_str().unwrap(), node))
        .collect();

    let leaf = *packages
        .iter()
        .find(|(_, name)| **name == "zeroship-workflow-schema")
        .expect("the workflow schema crate is a workspace member")
        .0;
    assert!(
        members.contains(leaf),
        "the leaf was resolved as a foreign package, so this walk proves nothing"
    );

    let mut reached = Vec::new();
    let mut pending = vec![leaf];
    let mut visited = BTreeSet::new();
    while let Some(id) = pending.pop() {
        if !visited.insert(id) {
            continue;
        }
        if id != leaf && members.contains(id) {
            reached.push(packages[id]);
        }
        for edge in nodes[id]["deps"].as_array().unwrap() {
            if edge["dep_kinds"]
                .as_array()
                .unwrap()
                .iter()
                .any(|kind| kind["kind"] != "dev")
            {
                pending.push(edge["pkg"].as_str().unwrap());
            }
        }
    }
    reached.sort_unstable();
    assert!(
        reached.is_empty(),
        "zeroship-workflow-schema must stay a leaf; it reaches {reached:?}"
    );
    assert!(
        visited.len() > 1,
        "the walk visited only the leaf itself, so an empty result is not evidence"
    );
}

fn workspace_metadata() -> serde_json::Value {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let output = Command::new(env!("CARGO"))
        .args(["metadata", "--format-version=1", "--all-features"])
        .current_dir(root)
        .output()
        .expect("resolve workspace dependencies");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

#[test]
fn workflow_process_dependencies_follow_crate_ownership() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
    let output = Command::new(env!("CARGO"))
        .args(["metadata", "--format-version=1", "--all-features"])
        .current_dir(root)
        .output()
        .expect("resolve workflow dependencies");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let metadata: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let packages: BTreeMap<_, _> = metadata["packages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|package| {
            (
                package["id"].as_str().unwrap(),
                package["name"].as_str().unwrap(),
            )
        })
        .collect();
    let nodes: BTreeMap<_, _> = metadata["resolve"]["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|node| (node["id"].as_str().unwrap(), node))
        .collect();
    for name in [
        "zeroship-workflow",
        "zeroship-workflow-calendar",
        "zeroship-workflow-client",
        "zeroship-workflow-manager",
        "zeroship-workflow-server",
        "zeroship-worker",
    ] {
        let id = *packages
            .iter()
            .find(|(_, package)| **package == name)
            .unwrap_or_else(|| panic!("missing {name}"))
            .0;
        let mut pending = vec![id];
        let mut visited = BTreeSet::new();
        while let Some(id) = pending.pop() {
            if !visited.insert(id) {
                continue;
            }
            let dependency = packages[id];
            if name == "zeroship-workflow-calendar" {
                assert!(
                    !["zeroship-core", "zeroship-workflow", "zeroship-workflow-manager", "zeroship-data-orm", "zeroship-storage", "compio", "tokio"]
                        .contains(&dependency),
                    "calendar calculation reaches a host or storage implementation through {dependency}"
                );
            }
            if matches!(name, "zeroship-workflow" | "zeroship-worker") {
                assert!(
                    !["zeroship-workflow-manager", "zeroship-workflow-server"]
                        .contains(&dependency),
                    "{name} reaches a platform workflow implementation through {dependency}"
                );
            }
            if matches!(
                name,
                "zeroship-workflow-manager" | "zeroship-workflow-server"
            ) {
                assert!(
                    !["zeroship-workflow", "zeroship-storage"].contains(&dependency),
                    "{name} reaches customer engine or payload storage through {dependency}"
                );
            }
            if name == "zeroship-workflow-client" {
                assert!(
                    !["zeroship-workflow", "zeroship-workflow-manager", "zeroship-data-orm", "zeroship-storage"]
                        .contains(&dependency),
                    "metadata client reaches engine or database implementation through {dependency}"
                );
            }
            if name != "zeroship-worker" {
                assert!(
                    ![
                        "v8",
                        "zeroship-runtime",
                        "zeroship-runtime-macros",
                        "zeroship-workflow-v8"
                    ]
                    .contains(&dependency),
                    "{name} reaches {dependency} through a shipped dependency"
                );
            }
            for edge in nodes[id]["deps"].as_array().unwrap() {
                if edge["dep_kinds"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|kind| kind["kind"] != "dev")
                {
                    pending.push(edge["pkg"].as_str().unwrap());
                }
            }
        }
    }
}
