//! Durable workflow journal, dispatch protocol, and app-scoped Rust backends.
//!
//! This crate has no dependency on V8. Customer hosts compose the shared engine
//! with their journal, retained executables and task executor.

#[cfg(test)]
extern crate self as zeroship_workflow;

pub mod advance;
pub mod apply;
pub mod backend;
pub mod calendar;
pub mod claim;
pub mod client;
pub mod coordination;
pub mod deployment_holds;
pub mod engine;
pub mod errors;
pub mod execution;
pub mod lifecycle;
pub mod operations;
pub mod service;
pub mod store;
pub mod validation;

pub use backend::{HttpWorkflowBackend, SharedWorkflowBackend, WorkflowBackend};
pub use client::{app_scoped_token, WorkflowClientConfig, WorkflowHttpMethod, WorkflowHttpRequest};
pub use errors::WorkflowServiceError;
pub use execution::{WorkflowExecution, WorkflowInvocation, WorkflowTrigger};
