//! The anti-vacuity guard for `src/registry.rs`.
//!
//! Every other check in this crate reads `platform_specs()`, and a binary that
//! is missing from that function is invisible to all of them: its declarations
//! are not linked, so no collision, no undeclared read and no name projection
//! can be attributed to it. Nothing about that state looks wrong from inside
//! the tool - every count simply gets smaller, and every check still passes.
//!
//! Cargo metadata is the independent second opinion. `check-metadata` already
//! requires every workspace bin target to carry a package-owned class, so the
//! set of targets classified `platform` is derived from the manifests rather
//! than from this crate, and comparing the two sets makes "forgot to register
//! the new server" a failure instead of a silent shrink.
//!
//! Does not cover: whether a linked registry declares the RIGHT names. That is
//! `contract.rs` plus `tests/linked_registry.rs`. This file only asks which
//! binaries are present on each side.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use zeroship_config_contract::contract::validate_contract;
use zeroship_config_contract::metadata::{extract_workspace, TargetClass};
use zeroship_config_contract::registry::{
    declared_binaries, platform_read_sites, platform_specs, DECLARING_BINARIES,
};

// There is no list of exempt platform targets here any more, and there is not
// meant to be one. Until 2026-08-21 this file carried
// `PLATFORM_TARGETS_WITHOUT_A_REGISTRY = ["zeroship-platform-migrate"]`, the
// one target the design put in scope and had not converted
// (`docs/proposals/2026-08-11-config-name-alignment.md:55-82`). Converting it
// left the constant holding nothing, and an empty allowance is worse than none:
// it reads as a place to add the next exception. The equality below is now
// between the manifests and the linked declarations, with nothing in between.

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("config-contract is two levels below the workspace root")
        .to_path_buf()
}

/// Every bin target the manifests classify `platform`.
fn platform_targets() -> BTreeSet<String> {
    let (_, classifications) = extract_workspace(&workspace_root().join("Cargo.toml"))
        .expect("cargo metadata classification");
    let targets = classifications
        .iter()
        .filter(|classification| classification.class == TargetClass::Platform)
        .map(|classification| classification.target.clone())
        .collect::<BTreeSet<_>>();
    // The instrument, not the subject: an extraction that returned nothing
    // would make every assertion below pass by comparing empty sets.
    assert!(
        !targets.is_empty(),
        "cargo metadata produced no platform targets; the comparisons below \
         would be vacuous"
    );
    targets
}

#[test]
fn every_platform_target_declares_its_configuration() {
    // Mutation: classify one more bin target `platform` in its Cargo.toml
    // without adding it to DECLARING_BINARIES. This is THE case the module doc
    // describes - an eighth platform process whose config nothing in this tool
    // can see.
    // Does not cover: a new platform process that is never given a bin target
    // or a manifest class at all. `check-metadata` owns that boundary.
    let declared = DECLARING_BINARIES
        .iter()
        .map(|target| (*target).to_owned())
        .collect::<BTreeSet<_>>();

    assert_eq!(
        platform_targets(),
        declared,
        "the `platform` targets in Cargo metadata and the binaries this tool \
         links have drifted; a new platform binary must link its generated \
         registry into src/registry.rs"
    );
}

#[test]
fn every_declaring_binary_is_classified_platform() {
    // Mutation: reclassify a server as `test-dev-tool` in its Cargo.toml. The
    // set equality above would still hold if BOTH lists were edited together,
    // so this states the direction that matters on its own: a binary this tool
    // treats as production must be production in the manifests too.
    // Does not cover: whether `platform` is the right class for a target. A
    // reason string is prose and cannot be checked.
    let platform = platform_targets();
    let missing = DECLARING_BINARIES
        .iter()
        .filter(|target| !platform.contains(**target))
        .collect::<Vec<_>>();

    assert!(
        missing.is_empty(),
        "{missing:?} link a registry into this tool but are not classified \
         `platform` in workspace metadata"
    );
}

#[test]
fn every_declaring_binary_actually_contributes_declarations() {
    // The other half of the vacuity: DECLARING_BINARIES is a list in this
    // crate, and a name on it whose registry is empty - a settings struct that
    // lost its fields, a crate that stopped invoking the attribute - would
    // shrink every count exactly as a missing binary does.
    // Mutation: delete a binary's `#[zeroship_config]` struct fields, or drop
    // its `SPECS` line from `platform_specs`.
    // Does not cover: how MANY declarations a binary owns. One field is enough
    // here; per-name coverage is tests/linked_registry.rs.
    let specs = platform_specs();
    assert!(!specs.is_empty(), "no linked declarations at all");
    let linked = declared_binaries(&specs).into_iter().collect::<BTreeSet<_>>();
    let expected = DECLARING_BINARIES
        .iter()
        .map(|target| (*target).to_owned())
        .collect::<BTreeSet<_>>();

    assert_eq!(
        linked, expected,
        "the binaries appearing in the LINKED declarations and the \
         DECLARING_BINARIES list disagree"
    );
}

#[test]
fn platform_declarations_match_their_compiled_readers() {
    let specs = platform_specs();
    let read_sites = platform_read_sites();

    validate_contract(&specs, &read_sites)
        .expect("the platform declarations and compiled readers must agree");
}
