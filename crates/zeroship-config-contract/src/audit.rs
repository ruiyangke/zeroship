//! Make the extraction disagree with the contract, or say so.
//!
//! `docs/reference/env-vars.md` is rendered from the COMPILED registries
//! ([`crate::registry`]). This module is the other half of the proposal's
//! Step 7 rule: the extraction stays, and its job becomes proving it reaches
//! the same answer.
//!
//! THE TWO DERIVATIONS SHARE NO CODE, and that is the only reason the
//! comparison is worth running:
//!
//!   compiled  the `zeroship_config` attribute expands in
//!             `crates/config-macros`, rustc compiles the `ConfigSpec`
//!             constants into each server library, and this tool LINKS them.
//!             Projections come from `CanonicalName::{env_name, flag_name,
//!             toml_path}` in `crates/core/src/config/names.rs`.
//!   extracted `crate::inventory` parses the same `.rs` files as TEXT with syn
//!             and re-implements the three projections itself
//!             (`inventory.rs:704-720`, both marked "mirroring").
//!
//! They share the source files on disk and the `syn` dependency. They share no
//! parser, no projection and no registry. A single mistake in either one
//! therefore surfaces as a difference rather than as agreement, which is what a
//! self-comparing check could never give.

use std::collections::BTreeSet;

use thiserror::Error;
use zeroship_core::config::{ConfigSpec, SupplyClass};

use crate::inventory::{InventoryRow, RowClass};

/// One comparable projection of one setting, as seen by one derivation.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Projection {
    /// Exact binary target.
    pub consumer: String,
    /// Canonical identity.
    pub canonical: String,
    /// Supply class spelling shared by both derivations.
    pub class: String,
    /// Long flag including its leading dashes.
    pub flag: String,
    /// Environment projection, or `-` when the class has no env tier.
    pub env: String,
    /// Overlay path, or `-` when the class has no overlay tier.
    pub toml: String,
}

/// A disagreement between the compiled contract and the source extraction.
#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum AuditError {
    /// One side produced nothing, so equality would be vacuous.
    #[error("{side} produced zero projections; an equality check would prove nothing")]
    Empty {
        /// Which derivation came back empty.
        side: &'static str,
    },
    /// The compiled contract declares something the extraction cannot see.
    #[error("compiled contract has a projection the source extraction does not: {0:?}")]
    OnlyCompiled(Projection),
    /// The extraction sees something the compiled contract does not declare.
    #[error("source extraction has a projection the compiled contract does not: {0:?}")]
    OnlyExtracted(Projection),
}

/// The class spelling both sides use.
const fn compiled_class(class: SupplyClass) -> &'static str {
    match class {
        SupplyClass::Operational => "operational",
        SupplyClass::Secret => "secret",
        SupplyClass::Bootstrap => "bootstrap",
        SupplyClass::Command => "command",
    }
}

/// Project the compiled registry into comparable rows.
#[must_use]
pub fn from_specs(specs: &[ConfigSpec]) -> BTreeSet<Projection> {
    let mut rows = BTreeSet::new();
    for spec in specs {
        for consumer in spec.consumers() {
            rows.insert(Projection {
                consumer: consumer.target().to_owned(),
                canonical: spec.canonical().as_str().to_owned(),
                class: compiled_class(spec.class()).to_owned(),
                flag: spec
                    .flag_name(*consumer)
                    .map_or_else(|| "-".to_owned(), |flag| format!("--{flag}")),
                env: spec.env_name().unwrap_or_else(|| "-".to_owned()),
                toml: spec
                    .toml_path()
                    .map_or_else(|| "-".to_owned(), ToOwned::to_owned),
            });
        }
    }
    rows
}

/// Project the source extraction into comparable rows.
///
/// Rows outside `binaries` are dropped: the inventory deliberately also
/// enumerates the fixture registries and the unconverted dev tools, neither of
/// which is part of the platform contract this compares.
#[must_use]
pub fn from_inventory(rows: &[InventoryRow], binaries: &[&str]) -> BTreeSet<Projection> {
    rows.iter()
        .filter(|row| binaries.contains(&row.consumer.as_str()))
        .filter(|row| row.class != RowClass::Unconverted)
        .map(|row| Projection {
            consumer: row.consumer.clone(),
            canonical: row.canonical.clone(),
            class: row.class.as_str().to_owned(),
            flag: row.flag.clone(),
            env: row.env.clone(),
            toml: row.toml.clone(),
        })
        .collect()
}

/// Require the two derivations to be equal in both directions.
///
/// # Errors
///
/// Returns [`AuditError::Empty`] when either side is empty, and one
/// `OnlyCompiled`/`OnlyExtracted` error per differing projection.
pub fn compare(
    compiled: &BTreeSet<Projection>,
    extracted: &BTreeSet<Projection>,
) -> Result<usize, Vec<AuditError>> {
    let mut errors = Vec::new();
    if compiled.is_empty() {
        errors.push(AuditError::Empty {
            side: "the compiled contract",
        });
    }
    if extracted.is_empty() {
        errors.push(AuditError::Empty {
            side: "the source extraction",
        });
    }
    if !errors.is_empty() {
        return Err(errors);
    }
    for row in compiled.difference(extracted) {
        errors.push(AuditError::OnlyCompiled(row.clone()));
    }
    for row in extracted.difference(compiled) {
        errors.push(AuditError::OnlyExtracted(row.clone()));
    }
    if errors.is_empty() {
        Ok(compiled.len())
    } else {
        Err(errors)
    }
}

#[cfg(test)]
mod tests {
    use super::{compare, from_inventory, from_specs, AuditError, Projection};
    use crate::inventory::{InventoryRow, RowClass, TomlEvidence};
    use zeroship_core::config::{CanonicalName, ConfigSpec, Consumer};

    const CONTROL: Consumer = Consumer::new("zeroship-control", "control");

    fn spec() -> ConfigSpec {
        ConfigSpec::operational(
            CanonicalName::from_static("control.port"),
            &[CONTROL],
            "port",
            "port",
            "u16",
            Some("9090"),
        )
    }

    fn row() -> InventoryRow {
        InventoryRow {
            consumer: "zeroship-control".to_owned(),
            package: "control".to_owned(),
            struct_name: "ControlSettings".to_owned(),
            field: "port".to_owned(),
            class: RowClass::Operational,
            flag: "--port".to_owned(),
            env: "ZEROSHIP_CONTROL_PORT".to_owned(),
            toml: "control.port".to_owned(),
            toml_evidence: TomlEvidence::Projection,
            canonical: "control.port".to_owned(),
            location: "crates/control/src/config.rs:1".to_owned(),
        }
    }

    #[test]
    fn two_derivations_of_the_same_declaration_agree() {
        let compiled = from_specs(&[spec()]);
        let extracted = from_inventory(&[row()], &["zeroship-control"]);
        assert_eq!(compare(&compiled, &extracted), Ok(1));
    }

    #[test]
    fn a_one_character_difference_in_either_projection_is_reported() {
        // The single most valuable mutation for this gate: change ONE
        // projection on ONE side. If the comparison shared a projection layer
        // this could not be constructed at all, which is the point of the
        // module header.
        let compiled = from_specs(&[spec()]);
        let mut drifted = row();
        drifted.env = "ZEROSHIP_CONTROL_PORTT".to_owned();
        let extracted = from_inventory(&[drifted], &["zeroship-control"]);
        let errors = compare(&compiled, &extracted).expect_err("a drifted env must fail");
        assert_eq!(errors.len(), 2, "one row missing on each side: {errors:?}");
        assert!(errors
            .iter()
            .any(|error| matches!(error, AuditError::OnlyCompiled(_))));
        assert!(errors
            .iter()
            .any(|error| matches!(error, AuditError::OnlyExtracted(_))));
    }

    #[test]
    fn an_empty_side_fails_instead_of_passing_vacuously() {
        let compiled = from_specs(&[spec()]);
        let errors = compare(&compiled, &std::collections::BTreeSet::new())
            .expect_err("an empty extraction must not read as agreement");
        assert_eq!(
            errors,
            vec![AuditError::Empty {
                side: "the source extraction"
            }]
        );
        let errors = compare(&std::collections::BTreeSet::new(), &compiled)
            .expect_err("an empty contract must not read as agreement");
        assert_eq!(
            errors,
            vec![AuditError::Empty {
                side: "the compiled contract"
            }]
        );
    }

    #[test]
    fn rows_outside_the_platform_binaries_are_not_compared() {
        // The fixture registries and dev-provision are real inventory rows.
        // Comparing them against the platform contract would fail forever, so
        // the filter is load-bearing rather than cosmetic - and this test is
        // what stops it from being widened into "drop anything inconvenient".
        let mut fixture = row();
        fixture.consumer = "zeroship-fixture-control".to_owned();
        let extracted = from_inventory(&[row(), fixture], &["zeroship-control"]);
        assert_eq!(extracted.len(), 1);
        let only: Vec<&Projection> = extracted.iter().collect();
        assert_eq!(only[0].consumer, "zeroship-control");
    }
}
