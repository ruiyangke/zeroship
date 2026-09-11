//! Shared workflow service for embedded and remote hosts.

mod app;
mod control;
mod frontier;
mod journal;
mod schedules;
pub use schedules::{
    IntervalAnchor, ScheduleCatchUp, ScheduleOverlap, ScheduleRegistration, ScheduleTiming,
};
mod tasks;
pub use app::{AppWorkflows, WorkflowService};
pub mod schema;
pub mod store;
mod types;
pub use types::*;

#[cfg(test)]
mod tests;
