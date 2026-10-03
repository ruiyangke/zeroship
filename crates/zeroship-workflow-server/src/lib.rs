//! Workflow metadata coordination, and the journal the creator-facing run
//! calls are answered from. Customer workers own execution; this service owns
//! the journal those runs are recorded in.

pub mod api;
pub mod auth;
pub mod config;
pub mod coordinator;
pub mod payloads;
pub mod publication;
pub mod runs;
pub mod server;
pub mod sweeps;

use std::{rc::Rc, sync::Arc};

#[derive(Debug)]
pub struct WorkflowHttpState {
    pub service: coordinator::Coordinator,
    pub auth: Arc<auth::WorkflowAuth>,
    /// Trusted finite policy observations; absent sources refuse lease requests.
    pub policy_source: Option<Rc<dyn zeroship_workflow_manager::policy::PolicySource>>,
    /// The journal creator-facing run calls read and write.
    ///
    /// Per thread, because the store it holds is bound to the runtime that
    /// opened it.
    pub runs: Rc<runs::RunService>,
    /// The object store a started run's input becomes an object in.
    ///
    /// A creator's start value crosses as a VALUE, so the process that admits the
    /// start is the process that stages it -- which is this one, because it owns
    /// both the journal row that names the object and the store the object lands
    /// in. Required rather than optional: the store is configured for every
    /// deployment, the sweep lane already refuses to start without it, and a
    /// `start` served without one would admit a run whose input nothing holds.
    ///
    /// Per thread, because the store is bound to the runtime that opened it.
    pub payloads: payloads::ServicePayloads,
}

pub fn configure(config: &mut ntex::web::ServiceConfig) {
    api::configure(config);
}

pub type SharedState = Rc<WorkflowHttpState>;
