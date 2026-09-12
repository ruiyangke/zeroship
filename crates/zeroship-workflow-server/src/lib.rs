//! Workflow metadata coordination. Customer workers own execution and storage.

pub mod api;
pub mod auth;
pub mod config;
pub mod coordinator;
pub mod server;

use std::{rc::Rc, sync::Arc};

#[derive(Debug)]
pub struct WorkflowHttpState {
    pub service: coordinator::Coordinator,
    pub auth: Arc<auth::WorkflowAuth>,
}

pub fn configure(config: &mut ntex::web::ServiceConfig) {
    api::configure(config);
}

pub type SharedState = Rc<WorkflowHttpState>;
