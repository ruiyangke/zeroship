//! Shared fixtures for the workflow-v8 integration contracts.
//!
//! `tests/integration/main.rs` declares this module once for the whole test
//! binary; every suite reaches a fixture through `crate::support::...` rather
//! than including the file a second time. `deployment_fixture` is the
//! repository-level app artifact fixture, included here so both suites that
//! need it share one copy.

pub mod orm;

#[path = "../../../../../tests/fixtures/workflow_deployments.rs"]
pub mod deployment_fixture;
