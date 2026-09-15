//! Durable workflow engine, app-scoped Rust backends and the creator host.
//!
//! This crate has no dependency on V8. Customer hosts compose the shared engine
//! with their journal, retained executables and task executor.

#[cfg(test)]
extern crate self as zeroship_workflow;

pub mod backend;
pub mod deployment_holds;
pub mod engine;
pub mod errors;
pub mod execution;
pub mod lifecycle;
pub mod operations;
pub mod service;
pub mod validation;

pub use backend::{SharedWorkflowBackend, WorkflowBackend};
pub use errors::WorkflowServiceError;
pub use execution::{WorkflowExecution, WorkflowInvocation, WorkflowTrigger};
