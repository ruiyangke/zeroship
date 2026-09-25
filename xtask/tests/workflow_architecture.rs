use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::process::Command;

/// The journal's schema crate must reach NOTHING else in the workspace.
///
/// That is the whole reason it exists. The migration service and the workflow
/// manager both depend on it, and a leaf lets them hold the journal's artifacts
/// while inheriting nothing else - which is what the schema-bundle path bought.
///
/// What a service may link BESIDE the artifacts is a separate question with its
/// own answer: `workflow_process_dependencies_follow_crate_ownership` above
/// names `zeroship-workflow-runner` and `zeroship-storage` as the edges the
/// manager and the server may not have, and says nothing about the engine.
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

/// The walk above follows only non-dev edges, so a `[dev-dependencies]` entry
/// is invisible to it - and that entry is exactly how payload bytes would
/// return to admission, as a contract that stages and reads objects there.
/// This reads what the manifest declares, of every dependency kind.
#[test]
fn the_workflow_admission_crate_declares_no_payload_storage_dependency() {
    let metadata = workspace_metadata();
    let package = metadata["packages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|package| package["name"] == "zeroship-workflow")
        .expect("zeroship-workflow is a workspace member");
    let declared: Vec<(&str, &str)> = package["dependencies"]
        .as_array()
        .unwrap()
        .iter()
        .map(|dependency| {
            (
                dependency["name"].as_str().unwrap(),
                dependency["kind"].as_str().unwrap_or("normal"),
            )
        })
        .collect();
    assert!(
        declared.iter().any(|(name, _)| *name == "zeroship-data-orm"),
        "no dependencies were read, so an empty result is not evidence"
    );
    let storage: Vec<_> = declared
        .iter()
        .filter(|(name, _)| *name == "zeroship-storage")
        .collect();
    assert!(
        storage.is_empty(),
        "zeroship-workflow declares payload storage: {storage:?}"
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
        "zeroship-workflow-runner",
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
                    !["zeroship-core", "zeroship-workflow", "zeroship-workflow-manager", "zeroship-workflow-runner", "zeroship-data-orm", "zeroship-storage", "compio", "tokio"]
                        .contains(&dependency),
                    "calendar calculation reaches a host or storage implementation through {dependency}"
                );
            }
            if matches!(
                name,
                "zeroship-workflow" | "zeroship-workflow-runner" | "zeroship-worker"
            ) {
                assert!(
                    !["zeroship-workflow-manager", "zeroship-workflow-server"]
                        .contains(&dependency),
                    "{name} reaches a platform workflow implementation through {dependency}"
                );
            }
            // Admission records which payload a run owns and proves that
            // ownership. The bytes are execution's: a writer, an opener and a
            // deleter arrive as arguments, supplied by the host that holds the
            // store. Reaching the store from here would let an execution path
            // grow back into admission behind a green gate.
            if name == "zeroship-workflow" {
                assert!(
                    dependency != "zeroship-storage",
                    "{name} reaches payload storage through {dependency}"
                );
            }
            if matches!(
                name,
                "zeroship-workflow-manager" | "zeroship-workflow-server"
            ) {
                assert!(
                    !["zeroship-workflow-runner", "zeroship-storage"].contains(&dependency),
                    "{name} reaches workflow execution or payload storage through {dependency}"
                );
            }
            if name == "zeroship-workflow-client" {
                assert!(
                    !["zeroship-workflow", "zeroship-workflow-manager", "zeroship-workflow-runner", "zeroship-data-orm", "zeroship-storage"]
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
        assert!(
            visited.len() > 1,
            "the walk for {name} visited only that package, so an empty result is not evidence"
        );
    }
}
