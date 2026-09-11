//! Shared workflow service for embedded and remote hosts.

mod app;
pub mod capability;
mod control;
mod deploys;
mod frontier;
mod ingress;
pub use ingress::{IngressReceipt, RevokedSignals, SignalAuthority, SignalTokenRequest};
mod journal;
mod payloads;
mod policy;
pub use policy::PlatformPolicy;
pub use payloads::{PayloadRead, PayloadSlot, StagedPayload};
mod remote;
pub use remote::{RemoteAppWorkflows, RemoteTasks, WorkflowEndpoint};
mod schedules;
mod signals;
pub use schedules::{
    IntervalAnchor, ScheduleCatchUp, ScheduleOverlap, ScheduleRegistration, ScheduleTiming,
};
pub use signals::AcceptedBroadcast;
mod tasks;
pub use app::{AppWorkflows, WorkflowService};
pub mod schema;
pub mod store;
mod types;
pub mod wire;
pub use types::*;

#[cfg(test)]
mod tests;
