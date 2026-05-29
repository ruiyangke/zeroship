use std::path::{Path, PathBuf};
use std::str::FromStr;

use cedar_policy::PolicySet;

fn main() {
    println!("cargo:rerun-if-changed=../../policies");

    let mut policy_files = Vec::new();
    walk_dir_for_cedar_files(Path::new("../../policies"), &mut policy_files);
    policy_files.sort();

    for path in policy_files {
        let source = std::fs::read_to_string(&path).expect("read .cedar file");
        if let Err(err) = PolicySet::from_str(&source) {
            panic!("policy parse error in {}: {err}", path.display());
        }
    }
}

fn walk_dir_for_cedar_files(dir: &Path, policy_files: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(dir).expect("read policies directory") {
        let entry = entry.expect("read policies directory entry");
        let path = entry.path();

        if path.is_dir() {
            walk_dir_for_cedar_files(&path, policy_files);
        } else if path.extension().is_some_and(|extension| extension == "cedar") {
            policy_files.push(path);
        }
    }
}
