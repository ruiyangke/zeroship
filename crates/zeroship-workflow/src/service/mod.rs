//! Shared workflow service for embedded and remote hosts.

mod app;
pub mod capability;
mod control;
mod frontier;
mod ingress;
pub use ingress::{IngressReceipt, RevokedSignals, SignalAuthority, SignalTokenRequest};
mod journal;
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
pub use types::*;

#[cfg(test)]
mod tests;
