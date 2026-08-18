//! Generated types and artifacts: what `gen-types` and `genArtifacts` emit from the single fold.
//!
//! A THEMED test binary. Every `mod` below was its own `tests/*.rs` integration
//! target until the whole directory was regrouped; each one statically linked its
//! own copy of the crate. Declaring them here makes them modules of ONE binary.
//! Nothing about the tests themselves changed - a `mod` missing from this list is a
//! test that silently stops running, so the list is the load-bearing part of the file.

#[macro_use]
#[path = "../support/mod.rs"]
mod support;

mod gen_artifacts_byte_identical;
mod gen_artifacts_references;
mod gen_types_authoring_tables_from_the_fold;
mod gen_types_field_defs_from_the_fold;
mod gen_types_mask_roundtrip;
mod gen_types_runtime_metadata_from_the_fold;
