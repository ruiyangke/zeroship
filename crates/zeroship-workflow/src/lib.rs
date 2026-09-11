//! Durable workflow journal, dispatch protocol, and app-scoped Rust backends.
//!
//! This crate has no dependency on V8. Hosts provide a workflow executor for
//! local SQLite runs; deployed runs use the control-plane HTTP backend.

pub mod advance;
pub mod apply;
pub mod backend;
pub mod claim;
pub mod client;
pub mod dev;
pub mod engine;
pub mod errors;
pub mod lifecycle;
pub mod operations;
pub mod store;
pub mod validation;

pub use backend::{HttpWorkflowBackend, SharedWorkflowBackend, WorkflowBackend};
pub use client::{app_scoped_token, WorkflowClientConfig, WorkflowHttpMethod, WorkflowHttpRequest};
pub use dev::{DevWorkflowEngine, WorkflowExecutor};
pub use errors::WorkflowServiceError;
