//! V8-free HTTP adapters for the shared transactional workflow service.

pub mod api;
pub mod auth;
pub mod config;
pub mod coordinator;
pub mod server;

use std::sync::Arc;
use zeroship_workflow::service::WorkflowService;

#[derive(Debug)]
pub struct WorkflowHttpState {
    pub service: WorkflowService,
    pub auth: auth::WorkflowAuth,
}

pub fn configure(config: &mut ntex::web::ServiceConfig) {
    api::configure(config);
}

pub type SharedState = Arc<WorkflowHttpState>;
