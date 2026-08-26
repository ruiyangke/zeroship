//! A crate's DIRECTORY must be spelled exactly like its PACKAGE.
//!
//! # Why this is a test and not a convention
//!
//! The rule is the one the owner chose when the engine's crates were folded into
//! the product workspace: *everything prefixed, directory == package name*. It is
//! worth a guard because a violation is INVISIBLE to every other check. Cargo does
//! not care - `path = "crates/anything"` resolves a package by reading the manifest
//! inside, so a directory may be named nothing like the package it holds and the
//! build stays green forever.
//!
//! The cost of that invisibility was paid, not imagined. Renaming the addon's
//! directory to `zeroship-migrate-node` while its `package.json` still said
//! `zero-migrate-node` left the npm workspace glob pointing at a path that no longer
//! existed: pnpm silently resolved 58 members instead of 59, dropped the addon two
//! packages declare as `workspace:*`, and reported success. Nothing failed. The
//! Cargo side of that same rename is guarded here so the equivalent cannot happen
//! quietly on the Rust side.
//!
//! # The two blindnesses, and the floor for each
//!
//! A census over a DISCOVERED set fails OPEN in two independent ways, and both look
//! identical from the outside - a green run.
//!
//! *Narrow the walk* and it iterates nothing. [`MEMBER_FLOOR`] defends that: if
//! `cargo metadata` hands back fewer members than any real state of either workspace
//! could have, the instrument is broken and the test says so rather than passing.
//!
//! *Break the comparison* and it walks every member and matches none.
//! [`the_comparison_can_actually_fail`] defends that by running the SAME
//! [`is_misnamed`] the census runs over synthetic pairs where the answer is known.
//! The positive control is deliberately NOT the census's own findings: those are
//! expected to be empty, and a check whose only proof-of-life is its own violations
//! goes blind at exactly the moment the codebase becomes correct.
//!
//! # Why completeness is checked separately
//!
//! [`every_crate_on_disk_is_a_member_or_declared_excluded`] answers a different
//! question: not *are the members named right* but *did cargo see every crate*. A
//! directory that quietly falls out of `members` would pass the naming census
//! vacuously, because a member cargo never reports is a member this file never
//! inspects. Its container directories are derived FROM cargo's own answer, so the
//! check adapts to whichever globs a workspace declares rather than hard-coding
//! `crates/`.
//!
//! That check cannot be falsified by editing THIS workspace, and the distinction is
//! worth stating rather than glossing: while `members` is the glob `crates/*`, every
//! directory holding a manifest is a member by construction, so the orphan state is
//! unreachable here. It becomes reachable the moment `members` is spelled out by
//! hand - which is the edit that would cause the loss it guards against. Its decision
//! is therefore proven the same way [`is_misnamed`] is, as a pure predicate over
//! inputs whose answer is known ([`the_orphan_rule_can_actually_fail`]), rather than
//! by a counterfactual this tree cannot produce.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;

/// The fewest workspace members either repository can honestly have.
///
/// The engine alone is nine crates and the product workspace is far larger, so any
/// number below this means the walk collapsed rather than that crates were deleted.
const MEMBER_FLOOR: usize = 8;

/// The property under test, extracted so it can be exercised on inputs whose answer
/// is known. `dir` is the directory holding the manifest; `package` is the name the
/// manifest declares.
fn is_misnamed(dir: &str, package: &str) -> bool {
    dir != package
}

/// The orphan decision, extracted for the same reason [`is_misnamed`] is: a
/// directory holding a manifest is a loss only when cargo does not count it AND the
/// workspace has not said so out loud.
fn is_orphan(holds_manifest: bool, is_member: bool, is_excluded: bool) -> bool {
    holds_manifest && !is_member && !is_excluded
}

/// The workspace root: the nearest ancestor whose `Cargo.toml` declares
/// `[workspace]`. Walking up rather than hard-coding a depth keeps this file
/// identical in both repositories, where the crate sits at a different depth.
fn workspace_root() -> PathBuf {
    let mut dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    loop {
        let manifest = dir.join("Cargo.toml");
        if manifest.is_file() {
            let text = std::fs::read_to_string(&manifest)
                .unwrap_or_else(|e| panic!("cannot read {}: {e}", manifest.display()));
            if text.lines().any(|l| l.trim_start().starts_with("[workspace]")) {
                return dir;
            }
        }
        assert!(
            dir.pop(),
            "walked past the filesystem root without finding a [workspace] manifest"
        );
    }
}

/// Every workspace member, as `package name -> manifest path`.
///
/// This asks CARGO rather than re-parsing the manifests, because cargo's answer is
/// the one that decides what gets built. A re-implementation of glob expansion could
/// agree with the manifests and still disagree with the build.
fn members() -> BTreeMap<String, PathBuf> {
    let root = workspace_root();
    let out = Command::new(std::env::var("CARGO").unwrap_or_else(|_| "cargo".into()))
        .args(["metadata", "--no-deps", "--format-version", "1", "--offline"])
        .arg("--manifest-path")
        .arg(root.join("Cargo.toml"))
        .output()
        .expect("cargo metadata could not be run");
    assert!(
        out.status.success(),
        "cargo metadata failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let json: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("cargo metadata emitted invalid JSON");
    let packages = json["packages"]
        .as_array()
        .expect("cargo metadata has no `packages` array");

    let mut found = BTreeMap::new();
    for pkg in packages {
        let name = pkg["name"]
            .as_str()
            .expect("a package has no name")
            .to_string();
        let manifest = PathBuf::from(
            pkg["manifest_path"]
                .as_str()
                .expect("a package has no manifest_path"),
        );
        found.insert(name, manifest);
    }
    found
}

/// The directory holding a manifest, as a string. Panics rather than defaulting,
/// because a silent empty string would compare equal to nothing and read as a
/// violation of a rule that was never actually measured.
fn holding_dir(manifest: &Path) -> String {
    manifest
        .parent()
        .and_then(Path::file_name)
        .and_then(|n| n.to_str())
        .unwrap_or_else(|| panic!("{} has no holding directory", manifest.display()))
        .to_string()
}

#[test]
fn every_workspace_member_lives_in_a_directory_named_for_its_package() {
    let members = members();

    assert!(
        members.len() >= MEMBER_FLOOR,
        "the member walk collapsed: cargo reported {} members, fewer than the {MEMBER_FLOOR} \
         any real state of this workspace has. The census below would pass vacuously.",
        members.len()
    );

    let offenders: Vec<String> = members
        .iter()
        .filter(|(name, manifest)| is_misnamed(&holding_dir(manifest), name))
        .map(|(name, manifest)| format!("  package `{name}` sits in `{}/`", holding_dir(manifest)))
        .collect();

    assert!(
        offenders.is_empty(),
        "{} workspace member(s) live in a directory that is not spelled like the package \
         (checked {} members):\n{}\n\nRename the DIRECTORY to match the package, or rename \
         the package. Cargo will not tell you: it resolves a path dependency by reading the \
         manifest inside, so the build stays green either way.",
        offenders.len(),
        members.len(),
        offenders.join("\n")
    );
}

#[test]
fn the_comparison_can_actually_fail() {
    assert!(
        is_misnamed("zeroship-migrate-cores", "zeroship-migrate-core"),
        "is_misnamed accepted a directory that differs from its package - the census \
         it powers would report zero offenders no matter what the tree looked like"
    );
    assert!(
        is_misnamed("migrate", "zeroship-migrate"),
        "is_misnamed accepted the prefix-dropping convention this workspace rejected"
    );
    assert!(
        !is_misnamed("zeroship-migrate-core", "zeroship-migrate-core"),
        "is_misnamed rejected an exactly-matching pair, so the census would fail on a \
         conformant tree and its green would mean nothing"
    );
}

#[test]
fn the_orphan_rule_can_actually_fail() {
    assert!(
        is_orphan(true, false, false),
        "is_orphan accepted a manifest cargo does not count and the workspace never \
         excluded - the completeness check it powers would report zero losses always"
    );
    assert!(
        !is_orphan(true, true, false),
        "is_orphan flagged an ordinary member, so the check would fail on every workspace"
    );
    assert!(
        !is_orphan(true, false, true),
        "is_orphan flagged a directory the workspace deliberately excluded"
    );
    assert!(
        !is_orphan(false, false, false),
        "is_orphan flagged a directory that holds no manifest at all"
    );
}

#[test]
fn every_crate_on_disk_is_a_member_or_declared_excluded() {
    let root = workspace_root();
    let members = members();

    // The container directories cargo actually draws members from (`crates/`,
    // `libs/`, ...), derived from its own answer rather than assumed.
    let containers: BTreeSet<PathBuf> = members
        .values()
        .filter_map(|m| m.parent()?.parent().map(Path::to_path_buf))
        .filter(|p| p != &root)
        .collect();
    assert!(
        !containers.is_empty(),
        "no container directories were derived from cargo's members, so the completeness \
         check below would inspect nothing"
    );

    let excluded: BTreeSet<PathBuf> = {
        let text = std::fs::read_to_string(root.join("Cargo.toml")).expect("unreadable root");
        let doc: toml::Value = text.parse().expect("root manifest is not valid TOML");
        doc.get("workspace")
            .and_then(|w| w.get("exclude"))
            .and_then(|e| e.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str())
                    .map(|s| root.join(s))
                    .collect()
            })
            .unwrap_or_default()
    };

    let known: BTreeSet<PathBuf> = members.values().cloned().collect();
    let mut orphans = Vec::new();
    for container in &containers {
        let entries = std::fs::read_dir(container)
            .unwrap_or_else(|e| panic!("cannot read {}: {e}", container.display()));
        for entry in entries.flatten() {
            let dir = entry.path();
            let manifest = dir.join("Cargo.toml");
            if !is_orphan(
                manifest.is_file(),
                known.contains(&manifest),
                excluded.contains(&dir),
            ) {
                continue;
            }
            orphans.push(dir.strip_prefix(&root).unwrap_or(&dir).display().to_string());
        }
    }

    assert!(
        orphans.is_empty(),
        "{} directory(ies) hold a Cargo.toml but are neither a workspace member nor listed \
         in `workspace.exclude`:\n  {}\n\nA crate that falls out of `members` is not built, \
         not tested, and passes the naming census vacuously - it is never inspected.",
        orphans.len(),
        orphans.join("\n  ")
    );
}
