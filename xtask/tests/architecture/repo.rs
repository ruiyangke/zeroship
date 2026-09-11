use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

pub fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .to_owned()
}
pub fn read(path: &str) -> String {
    std::fs::read_to_string(root().join(path)).unwrap_or_else(|e| panic!("read {path}: {e}"))
}
pub fn files(directory: &str, extensions: &[&str]) -> Vec<PathBuf> {
    fn walk(directory: &Path, extensions: &[&str], output: &mut Vec<PathBuf>) {
        for entry in std::fs::read_dir(directory).unwrap() {
            let entry = entry.unwrap();
            if entry.file_type().unwrap().is_dir() {
                if matches!(
                    entry.file_name().to_str(),
                    Some("target" | "dist" | "node_modules" | ".git" | "wpt" | ".zeroship")
                ) {
                    continue;
                }
                walk(&entry.path(), extensions, output);
            } else if entry
                .path()
                .extension()
                .and_then(|v| v.to_str())
                .is_some_and(|ext| extensions.contains(&ext))
            {
                output.push(entry.path());
            }
        }
    }
    let mut output = Vec::new();
    walk(&root().join(directory), extensions, &mut output);
    output.sort();
    output
}

pub fn metadata() -> &'static Value {
    static METADATA: OnceLock<Value> = OnceLock::new();
    METADATA.get_or_init(|| {
        let output = Command::new(env!("CARGO"))
            .current_dir(root())
            .args(["metadata", "--format-version", "1"])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "cargo metadata failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    })
}
pub fn package(name: &str) -> &'static Value {
    metadata()["packages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["name"] == name)
        .unwrap_or_else(|| panic!("missing package {name}"))
}
pub fn workspace() -> Vec<&'static Value> {
    metadata()["workspace_members"]
        .as_array()
        .unwrap()
        .iter()
        .map(|id| {
            metadata()["packages"]
                .as_array()
                .unwrap()
                .iter()
                .find(|p| p["id"] == *id)
                .unwrap()
        })
        .collect()
}
pub fn normal_closure(name: &str) -> BTreeSet<String> {
    let packages: BTreeMap<_, _> = metadata()["packages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| (p["id"].as_str().unwrap(), p["name"].as_str().unwrap()))
        .collect();
    let nodes: BTreeMap<_, _> = metadata()["resolve"]["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .map(|n| (n["id"].as_str().unwrap(), n))
        .collect();
    let mut pending = vec![package(name)["id"].as_str().unwrap()];
    let mut seen = BTreeSet::new();
    while let Some(id) = pending.pop() {
        if !seen.insert(id) {
            continue;
        }
        for dependency in nodes[id]["deps"].as_array().unwrap() {
            if dependency["dep_kinds"]
                .as_array()
                .unwrap()
                .iter()
                .any(|kind| kind["kind"].is_null())
            {
                pending.push(dependency["pkg"].as_str().unwrap());
            }
        }
    }
    assert!(!seen.is_empty());
    seen.into_iter().map(|id| packages[id].to_owned()).collect()
}
