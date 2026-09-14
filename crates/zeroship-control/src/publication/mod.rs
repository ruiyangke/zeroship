//! Normal deployment publication: deploy command receipts, app lifecycle
//! revisions and the intents Control delivers to the workflow manager.
//!
//! [`catalog`] commits each app lifecycle change together with its intent in
//! one ORM transaction on the Control database. The deployment collector treats
//! a pending activation as a dependency of its bundle.

#![expect(
    clippy::future_not_send,
    reason = "Control ORM transactions and manager exchanges stay on their compio thread"
)]

pub mod catalog;
pub mod command;
pub mod models;

pub use catalog::{CatalogError, Transition};
pub use command::{
    normalize_content_type, Acceptance, AcceptanceResult, CommandBinding, DeployCommand,
    DeploymentRejected, VerifiedDeployment, DEPLOY_OPERATION, ZSHIP_CONTENT_TYPE,
};

/// Wall-clock milliseconds for catalog timestamps. Control replicas share the
/// platform clock discipline; ordering never depends on these values alone.
#[must_use]
pub fn now_millis() -> i64 {
    chrono::Utc::now().timestamp_millis()
}
