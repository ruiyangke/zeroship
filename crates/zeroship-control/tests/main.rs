#![recursion_limit = "256"]

//! Integration modules grouped to share a linked executable. Database modules
//! live in tests/live_db.rs; both targets run in ordinary cargo test.
//! Register new modules here or there because autotests is disabled.
//!
//! Workflow engine tests keep a separate executable so database cloning cannot
//! conflict with sibling modules holding sessions on its template.
//! The source residue guard also needs its own target.

mod common;

mod billing_pipeline_redpanda_e2e;
mod config_env_tier;
mod provider_conformance;
mod trusted_clients_test;
