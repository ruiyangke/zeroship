//! Complete raw workflow policy shared by platform and creator hosts.
//!
//! Source revisions identify these unchanged values. Lease masking and monotonic
//! deadlines belong to the host and must not be serialized as source policy.

use serde::{Deserialize, Serialize};

mod lease;
pub use lease::{EstablishIngress, PolicyLease, PolicyLeaseRequest};

/// Hard ceiling shared by policy validation and signal capability verification.
pub const SIGNAL_CAPABILITY_MAX_LIFETIME_SECONDS: i64 = 604_800;

/// The complete policy violates an admission or resource constraint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[error("invalid workflow admission policy")]
pub struct InvalidPolicy;

/// Raw values are explicit and complete; missing fields never select defaults.
///
/// Hosts validate deserialized values before admitting authority. Unsigned JSON
/// integers must fit the receiving host: reject overflow instead of truncating
/// or clamping limits stored as `usize`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AppPolicy {
    pub admission: bool,
    pub dispatch: bool,
    pub ingress: bool,
    pub max_live_runs: i64,
    pub max_child_depth: i64,
    pub max_running: i64,
    pub max_input_bytes: usize,
    pub max_frontier: usize,
    pub max_journal_bytes: usize,
    pub max_payload_bytes: i64,
    pub max_payload_objects: i64,
    pub max_payload_storage_bytes: i64,
    pub payload_staging_retention_ms: i64,
    /// How many times creator code may execute at one journal ordinal before
    /// the platform stops re-running it.
    ///
    /// One budget covers both phases, because both are the same question about
    /// the same ordinal: a `step.run` body re-executed under its declared
    /// `retries`, and a compensator re-executed after it reported a failure.
    /// A per-step declaration above this ceiling is refused rather than clamped,
    /// so a creator learns their policy was rejected instead of silently
    /// getting a smaller one.
    pub max_step_attempts: i32,
    /// How long a run waits before the next attempt at that ordinal.
    pub retry_delay_ms: i64,
    /// How many delivery attempts of one job may reach creator execution before
    /// the manager stops redelivering it. Attempts the worker never began, such
    /// as a claim the creator journal deferred, are not counted against it.
    pub max_delivery_attempts: i64,
    /// How many consecutive dispatches of one run may be reclaimed without the
    /// executor reporting an outcome before the creator engine gives up on it.
    /// A dispatch that commits a frontier transition supersedes the frontier the
    /// strikes were counted against, so the count starts again from the
    /// transition rather than from the run.
    ///
    /// One budget covers both phases, because both are the same question about
    /// the same dispatch path. Where they differ is the resting state: a forward
    /// run the host gave up on settles `stalled`, and a rollback it gave up on
    /// rests at the failure it was rolling back with the undischarged
    /// obligations named on the compensation summary.
    ///
    /// Strikes advance no faster than the counted executions bounding
    /// `max_delivery_attempts`, so keeping this below that ceiling is what
    /// leaves the engine's verdict reachable at all: past the ceiling the
    /// manager stops delivering and no dispatch remains to strike.
    pub max_stuck_dispatches: i64,
    pub max_schedules: usize,
    pub max_schedule_backfill: usize,
    pub min_schedule_interval_ms: i64,
    pub max_signal_token_lifetime_seconds: i64,
    pub lease_ms: i64,
}
impl Default for AppPolicy {
    fn default() -> Self {
        Self {
            admission: true,
            dispatch: true,
            ingress: true,
            max_live_runs: 10_000,
            max_child_depth: 16,
            max_running: 16,
            max_input_bytes: 1024 * 1024,
            max_frontier: 256,
            max_journal_bytes: 16 * 1024 * 1024,
            max_payload_bytes: 64 * 1024 * 1024,
            max_payload_objects: 100_000,
            max_payload_storage_bytes: 1024 * 1024 * 1024,
            payload_staging_retention_ms: 86_400_000,
            max_step_attempts: 8,
            retry_delay_ms: 1_000,
            max_delivery_attempts: 8,
            max_stuck_dispatches: 4,
            max_schedules: 64,
            max_schedule_backfill: 32,
            min_schedule_interval_ms: 1_000,
            max_signal_token_lifetime_seconds: 86_400,
            lease_ms: 60_000,
        }
    }
}
impl AppPolicy {
    /// Validate the complete raw policy before granting authority.
    ///
    /// # Errors
    /// Rejects invalid limits and signal lifetimes beyond the capability ceiling.
    pub const fn validate(&self) -> Result<(), InvalidPolicy> {
        if self.max_live_runs < 0
            || self.max_child_depth < 0
            || self.max_running < 0
            || self.max_input_bytes == 0
            || self.max_frontier == 0
            || self.max_journal_bytes == 0
            || self.max_payload_bytes <= 0
            || self.max_payload_objects <= 0
            || self.max_payload_storage_bytes < self.max_payload_bytes
            || self.payload_staging_retention_ms <= 0
            || self.max_step_attempts <= 0
            || self.retry_delay_ms <= 0
            || self.max_delivery_attempts <= 0
            || self.max_stuck_dispatches <= 0
            || self.max_stuck_dispatches >= self.max_delivery_attempts
            || self.max_schedule_backfill == 0
            || self.min_schedule_interval_ms <= 0
            || self.max_signal_token_lifetime_seconds <= 0
            || self.max_signal_token_lifetime_seconds > SIGNAL_CAPABILITY_MAX_LIFETIME_SECONDS
            || self.lease_ms <= 0
        {
            return Err(InvalidPolicy);
        }
        Ok(())
    }
}
