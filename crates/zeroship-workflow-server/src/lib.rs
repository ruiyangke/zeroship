//! Workflow metadata coordination. Customer workers own execution and storage.

pub mod api;
pub mod auth;
pub mod config;
pub mod coordinator;
pub mod journal;
pub mod server;

use std::{rc::Rc, sync::Arc};

#[derive(Debug)]
pub struct WorkflowHttpState {
    pub service: coordinator::Coordinator,
    pub auth: Arc<auth::WorkflowAuth>,
    /// Trusted finite policy observations; absent sources refuse lease requests.
    pub policy_source: Option<Rc<dyn zeroship_workflow_manager::policy::PolicySource>>,
    /// Sends the journal bundle to the migration service.
    ///
    /// Absent when no migration-service origin is configured, which refuses the
    /// journal endpoint rather than silently leaving journals unprovisioned.
    pub journal: Option<journal::Journal>,
}

pub fn configure(config: &mut ntex::web::ServiceConfig) {
    api::configure(config);
}

pub type SharedState = Rc<WorkflowHttpState>;
