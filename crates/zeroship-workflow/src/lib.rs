//! Durable workflow engine, app-scoped Rust backends and the creator host.
//!
//! This crate has no dependency on V8. Customer hosts compose the shared engine
//! with their journal, retained executables and task executor.

#[cfg(test)]
extern crate self as zeroship_workflow;

pub mod backend;
pub mod deploy_registrations;
pub mod deployment_holds;
pub mod engine;
pub mod errors;
pub mod execution;
/// The workflow-typed fixture adapters this crate's unit tests share, expanded
/// from `zeroship-workflow-testkit` against this crate itself.
#[cfg(test)]
#[expect(
    clippy::future_not_send,
    reason = "the fixtures' journals and bindings stay on their owning compio thread"
)]
pub mod fixtures {
    zeroship_workflow_testkit::workflow_fixtures!();
}
pub mod lifecycle;
pub mod operations;
pub mod service;
pub mod validation;

pub use backend::{
    InputStager, SharedInputStager, SharedStepOutputs, SharedWorkflowBackend, StepOutputReader,
    WorkflowBackend,
};
pub use errors::WorkflowServiceError;
pub use execution::{WorkflowExecution, WorkflowInvocation, WorkflowTrigger};
