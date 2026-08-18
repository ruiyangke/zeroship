//! The live PostgreSQL engine: apply, declarative deploy, project locks, preconditions and timeouts.
//!
//! A THEMED test binary. Every `mod` below was its own `tests/*.rs` integration
//! target until the whole directory was regrouped; each one statically linked its
//! own copy of the crate. Declaring them here makes them modules of ONE binary.
//! Nothing about the tests themselves changed - a `mod` missing from this list is a
//! test that silently stops running, so the list is the load-bearing part of the file.

#[macro_use]
#[path = "../support/mod.rs"]
mod support;

mod advisories_match_live_postgres;
mod apply_dml_validation_pg;
mod not_valid_validate_constraint;
mod pg_column_drop_dependency_oracle;
mod pg_conformance;
mod pg_declarative;
mod pg_drop_column_dependency_guard;
mod pg_plan_precondition_preflight;
mod pg_primary_key;
mod pg_scenarios;
mod precondition_evaluation_pg;
mod timeout_budget_pg;
