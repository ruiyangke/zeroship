//! Shared fixtures for the workflow-v8 integration contracts.
//!
//! `tests/main.rs` declares this module once for the whole test binary; every
//! suite reaches a fixture through `crate::support::...` rather than including
//! the file a second time. `deployment_fixture` is the workflow-typed deployment
//! catalog from `zeroship-workflow-fixtures`, re-exported here so both suites
//! that need it reach it by one name.

pub mod orm;
pub mod unreachable;

pub use zeroship_workflow_fixtures::deployment as deployment_fixture;
