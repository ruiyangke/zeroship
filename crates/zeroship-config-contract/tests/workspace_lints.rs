//! Every first-party workspace member must inherit the shared lint table.
//!
//! Section 4.5 of `docs/proposals/2026-08-11-config-name-alignment.md`: the
//! compiler-enforced environment-access policy is
//! `[workspace.lints.clippy] disallowed_methods = "deny"`, and a member that
//! omits `[lints] workspace = true` simply does not get it. That omission is
//! invisible - the crate compiles, Clippy runs, and the deny is silently absent
//! for that crate alone. There is no per-crate opt-out.
//!
//! This reads `cargo metadata` for the member list rather than globbing
//! `crates/*/Cargo.toml`, because the member set is what Cargo actually builds:
//! a directory that is not a member would be checked and never linted, and a
//! member outside those globs would be missed entirely.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use cargo_metadata::MetadataCommand;

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("crates/zeroship-config-contract has a workspace root above it")
        .to_path_buf()
}

fn member_manifests() -> Vec<(String, PathBuf)> {
    let root = workspace_root();
    let metadata = MetadataCommand::new()
        .manifest_path(root.join("Cargo.toml"))
        .no_deps()
        .exec()
        .expect("cargo metadata");
    metadata
        .workspace_packages()
        .into_iter()
        .map(|package| {
            (
                package.name.to_string(),
                PathBuf::from(package.manifest_path.as_std_path()),
            )
        })
        .collect()
}

/// Whether a manifest inherits the workspace lint table.
///
/// A TOML parse rather than a text search: `[lints]\nworkspace = true` can be
/// written as an inline table, and `workspace = true` appears in this file
/// dozens of times under `[dependencies]`. A `grep` would agree with almost
/// every manifest for the wrong reason.
fn inherits_workspace_lints(manifest: &Path) -> bool {
    let source = std::fs::read_to_string(manifest).expect("read member manifest");
    let document: toml::Value = toml::from_str(&source).expect("parse member manifest");
    document
        .get("lints")
        .and_then(|lints| lints.get("workspace"))
        .and_then(toml::Value::as_bool)
        .unwrap_or(false)
}

#[test]
fn every_workspace_member_inherits_the_shared_lints() {
    let members = member_manifests();

    // Anti-vacuity: the real workspace has both `crates/*` and `libs/*`
    // members. A metadata call that returned two packages would otherwise pass.
    assert!(
        members.len() > 20,
        "cargo metadata reported only {} workspace members; the enumeration is \
         broken and a clean result would prove nothing",
        members.len()
    );

    let missing = members
        .iter()
        .filter(|(_, manifest)| !inherits_workspace_lints(manifest))
        .map(|(name, _)| name.clone())
        .collect::<BTreeSet<_>>();

    assert!(
        missing.is_empty(),
        "these workspace members do not inherit the shared lint table, so \
         `disallowed_methods = \"deny\"` does not apply to them:\n  {}\n\nAdd \
         `[lints]\\nworkspace = true` to each manifest.",
        missing.into_iter().collect::<Vec<_>>().join("\n  ")
    );
}

#[test]
fn a_member_without_the_lint_table_is_detected() {
    // Mutation, applied in-memory: the check must distinguish a manifest that
    // inherits from one that does not. Without this, `inherits_workspace_lints`
    // could return `true` unconditionally and the test above would still pass.
    // Does not cover: a member whose manifest sets `[lints] workspace = false`
    // AND redefines the deny locally. That would satisfy the intent and fail
    // this gate; nothing in the tree does it, and the rule is "no per-crate
    // opt-out" precisely so the question does not arise.
    let directory = std::env::temp_dir().join(format!(
        "zeroship-workspace-lints-{}-{}",
        std::process::id(),
        line!()
    ));
    std::fs::create_dir_all(&directory).expect("scratch directory");

    let without = directory.join("without.toml");
    std::fs::write(&without, "[package]\nname = \"x\"\nversion = \"0.1.0\"\n")
        .expect("write manifest without lints");
    assert!(!inherits_workspace_lints(&without));

    let with = directory.join("with.toml");
    std::fs::write(
        &with,
        "[package]\nname = \"x\"\nversion = \"0.1.0\"\n\n[lints]\nworkspace = true\n",
    )
    .expect("write manifest with lints");
    assert!(inherits_workspace_lints(&with));

    let _ = std::fs::remove_dir_all(&directory);
}

#[test]
fn the_shared_lint_table_actually_denies_the_raw_methods() {
    // The gate above proves every member INHERITS the table. That is worthless
    // if the table stopped containing the deny, which is a one-line edit in a
    // file nobody re-reads. This asserts the content, and the clippy.toml that
    // names the methods, so the two halves cannot drift apart.
    let root = workspace_root();
    let manifest: toml::Value =
        toml::from_str(&std::fs::read_to_string(root.join("Cargo.toml")).expect("root manifest"))
            .expect("parse root manifest");
    let level = manifest
        .get("workspace")
        .and_then(|workspace| workspace.get("lints"))
        .and_then(|lints| lints.get("clippy"))
        .and_then(|clippy| clippy.get("disallowed_methods"))
        .and_then(toml::Value::as_str);
    assert_eq!(
        level,
        Some("deny"),
        "[workspace.lints.clippy] must deny disallowed_methods"
    );

    let clippy: toml::Value =
        toml::from_str(&std::fs::read_to_string(root.join("clippy.toml")).expect("clippy.toml"))
            .expect("parse clippy.toml");
    let listed = clippy
        .get("disallowed-methods")
        .and_then(toml::Value::as_array)
        .expect("disallowed-methods array")
        .iter()
        .filter_map(|entry| entry.get("path").and_then(toml::Value::as_str))
        .collect::<BTreeSet<_>>();
    for method in [
        "std::env::var",
        "std::env::var_os",
        "std::env::vars",
        "std::env::vars_os",
    ] {
        assert!(listed.contains(method), "{method} is not disallowed");
    }
}
