//! The authoring surface offline: analyzer, classifier, expression coverage, render shapes and load scaling.
//!
//! Declaring each `mod` below is load-bearing: a module missing from this list is a
//! suite that silently stops running.


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
