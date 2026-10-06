//! Shared fixtures for the workflow-v8 integration contracts.
//!
//! `tests/main.rs` declares this module once for the whole test binary; every
//! suite reaches a fixture through `crate::support::...` rather than including
//! the file a second time. `workflow_fixtures` expands the workflow-typed
//! adapters from `zeroship-workflow-testkit` against `zeroship-workflow`;
//! `deployment_fixture` is its deployment catalog, re-exported here so both
//! suites that need it reach it by one name.

pub mod orm;
pub mod unreachable;

#[expect(
    clippy::future_not_send,
    reason = "the fixtures' journals and bindings stay on their owning compio thread"
)]
mod workflow_fixtures {
    zeroship_workflow_testkit::workflow_fixtures!();
}

pub use workflow_fixtures::deployment as deployment_fixture;
