//! The workflow-typed fixture adapters for the crates downstream of
//! `zeroship-workflow`.
//!
//! [`zeroship_workflow_testkit::workflow_fixtures!`] holds the adapters once;
//! this crate expands them against `zeroship-workflow` so the runner, the
//! worker and the V8 binding share one compiled copy. `zeroship-workflow`'s own
//! unit tests expand them in place, against the crate under test.

#![expect(
    clippy::future_not_send,
    reason = "the fixtures' journals and bindings stay on their owning compio thread"
)]

zeroship_workflow_testkit::workflow_fixtures!();
