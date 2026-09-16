//! Normal deployment publication: deploy command receipts, app lifecycle
//! revisions and the intents Control delivers to the workflow manager.
//!
//! [`catalog`] commits each app lifecycle change together with its intent in
//! one ORM transaction on the Control database. [`shared`] runs those
//! transactions for every request on the process's bounded set of catalog
//! threads, each holding one session. [`publisher`] later
//! delivers pending intents in per-app revision order and records only the
//! exact receipt the manager returned. The deployment collector treats a
//! pending activation as a dependency of its bundle.
//!
//! [`journal`] is the one manager call the deploy REQUEST makes rather than
//! leaving to the publisher: an app that declares workflows must carry a
//! current journal before its activation intent commits, and a failure to
//! provision one belongs in the deploy's answer rather than in a retry queue
//! the creator cannot see.

#![expect(
    clippy::future_not_send,
    reason = "Control ORM transactions and manager exchanges stay on their compio thread"
)]

pub mod catalog;
pub mod command;
pub mod journal;
pub mod models;
pub mod publisher;
pub mod shared;

use zeroship_data_orm::orm::UtcInstant;

pub use catalog::{CatalogError, Transition};
pub use journal::{DeployJournal, JournalError, JournalManager};
pub use shared::{Catalog, CatalogOptions, Closing};
pub use command::{
    normalize_content_type, Acceptance, AcceptanceResult, CommandBinding, DeployCommand,
    DeploymentRejected, VerifiedDeployment, DEPLOY_OPERATION, ZSHIP_CONTENT_TYPE,
};

/// The wall-clock instant catalog timestamp columns take. Control replicas
/// share the platform clock discipline; ordering never depends on these values
/// alone.
///
/// # Errors
/// Refuses a process clock outside the portable calendar rather than storing a
/// timestamp the database cannot represent.
pub fn now() -> Result<UtcInstant, zeroship_data_orm::error::DbError> {
    UtcInstant::from_unix_micros(chrono::Utc::now().timestamp_micros())
}
