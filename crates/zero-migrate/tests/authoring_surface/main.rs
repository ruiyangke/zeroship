//! The authoring surface offline: analyzer, classifier, expression coverage, render shapes and load scaling.
//!
//! A THEMED test binary. Every `mod` below was its own `tests/*.rs` integration
//! target until the whole directory was regrouped; each one statically linked its
//! own copy of the crate. Declaring them here makes them modules of ONE binary.
//! Nothing about the tests themselves changed - a `mod` missing from this list is a
//! test that silently stops running, so the list is the load-bearing part of the file.

#[macro_use]
#[path = "../support/mod.rs"]
mod support;

mod analyze;
mod classify;
mod composite_foreign_keys;
mod expr_equivalence_coverage;
mod f664_scaling;
mod f665_scaling;
mod partition_absence_equivalence_index;
mod partition_render;
mod sequences_exclusion;
mod views;
