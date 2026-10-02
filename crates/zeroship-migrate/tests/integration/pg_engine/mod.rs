//! The live PostgreSQL engine: apply, declarative deploy, project locks, preconditions and timeouts.
//!
//! Declaring each `mod` below is load-bearing: a module missing from this list is a
//! suite that silently stops running.


mod advisories_match_live_postgres;
mod apply_dml_validation_pg;
mod not_valid_validate_constraint;
mod owned_server;
mod pg_column_drop_dependency_oracle;
mod pg_conformance;
mod pg_declarative;
mod pg_drop_column_dependency_guard;
mod pg_plan_precondition_preflight;
mod pg_primary_key;
mod pg_recorded_engine_paths;
mod pg_scenarios;
mod precondition_evaluation_pg;
mod timeout_budget_pg;
