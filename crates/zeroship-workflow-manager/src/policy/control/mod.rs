//! Finite, revisioned policy authority from Control's own database.
//!
//! The ledger orders authoritative observations. It does not record unobserved
//! intermediate changes or make operator acknowledgements prove worker quiescence.
#![expect(
    clippy::future_not_send,
    reason = "native policy storage stays on its compio thread"
)]

use super::{PolicyObservation, PolicySource};
use crate::Error;
use std::{
    future::Future,
    pin::Pin,
    time::{Duration, Instant},
};

mod cache;
mod models;
mod store;
pub use cache::PolicyObservations;
pub use store::{ControlPolicyStore, RolloutPolicy};

/// Native metadata for the Control binding. Source projections contain only
/// policy contributors; the publication ledger contains no creator payloads.
///
/// # Errors
/// Refuses invalid native model declarations.
pub fn collections() -> Result<zeroship_data_orm::schema::Schema, Error> {
    let schema = models::schema::schema();
    schema.validate()?;
    Ok(schema)
}

/// A thread's store bound to the process-wide observations every thread shares.
///
/// Entries never renew their original source validity. Concurrent misses for
/// the same app return a retryable failure while its bounded database
/// observation is pending. No detached refresh survives cancellation.
#[derive(Debug)]
pub struct ControlPolicies {
    store: ControlPolicyStore,
    observations: PolicyObservations,
    read_timeout: Duration,
}

impl ControlPolicies {
    /// Bind a native store with finite I/O to this process's observations.
    ///
    /// The observations are supplied rather than created here: every thread of
    /// one manager must answer for an app from the same observation, or the
    /// deadline a host is granted moves backwards whenever its lease lands on
    /// another thread.
    ///
    /// # Errors
    /// Rejects zero or unrepresentable timeouts.
    pub fn new(
        store: ControlPolicyStore,
        observations: PolicyObservations,
        read_timeout: Duration,
    ) -> Result<Self, Error> {
        if read_timeout.is_zero() || Instant::now().checked_add(read_timeout).is_none() {
            return Err(Error::Invalid);
        }
        Ok(Self {
            store,
            observations,
            read_timeout,
        })
    }

    /// Retire local captured authority and force the next caller to read Control.
    /// A delayed refresh cannot restore the removed entry. Other replicas remain
    /// bounded by their original observations; this is not a fleet-wide barrier.
    pub fn invalidate(&self, app: &zeroship_core::app_id::AppId) {
        self.observations.cache().invalidate(app);
    }
}

impl PolicySource for ControlPolicies {
    fn observe<'a>(
        &'a self,
        app: &'a zeroship_core::app_id::AppId,
    ) -> Pin<Box<dyn Future<Output = Result<PolicyObservation, Error>> + 'a>> {
        Box::pin(async move {
            match self.observations.cache().reserve(app)? {
                cache::Reservation::Cached(observation) => Ok(observation),
                cache::Reservation::Refresh(ticket) => {
                    let observation =
                        compio::time::timeout(self.read_timeout, self.store.observe(app))
                            .await
                            .map_err(|_| Error::Unavailable)?;
                    ticket.complete(observation?)
                }
            }
        })
    }

    fn revalidate(&self, observation: &PolicyObservation) -> Result<Instant, Error> {
        self.observations.cache().revalidate(observation)
    }
}
