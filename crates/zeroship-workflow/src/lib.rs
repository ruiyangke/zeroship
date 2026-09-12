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
pub mod store;

pub use backend::{HttpWorkflowBackend, SharedWorkflowBackend, WorkflowBackend};
pub use client::{
    app_scoped_token, WorkflowClientConfig, WorkflowHttpMethod, WorkflowHttpRequest,
    WorkflowRpcError,
};
pub use dev::{DevWorkflowEngine, WorkflowExecutor};
