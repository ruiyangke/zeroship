use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::process::Command;

#[test]
fn rust_workflow_crates_do_not_depend_on_the_v8_runtime() {
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
    for name in ["zeroship-workflow", "zeroship-workflow-scheduler"] {
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
