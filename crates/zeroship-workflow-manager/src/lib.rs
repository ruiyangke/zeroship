//! Workflow scheduling and durable delivery using platform-bound ORM storage.
#![cfg_attr(test, recursion_limit = "256")]

pub mod app_facts;
pub mod capacity;
mod clock;
pub mod coordinator;
pub mod deployments;
pub mod driver;
pub mod eligibility;
mod error;
pub mod lifecycle;
pub mod local;
pub mod maintenance;
mod models;
mod management;
pub mod policy;
mod queue;
pub mod recovery;
pub mod retention;
pub mod scheduling;

pub use error::Error;
pub use models::{collections, Claimant};
/// The manager's native ORM table handles, for hosts that query the native
/// schema directly. `collections` is the metadata a host composes into its
/// binding.
pub use models::schema;
pub use queue::{DeliveryGrant, Options, Queue};
