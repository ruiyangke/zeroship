//! Zone-scoped workflow claims and management share the queue's app lock.
#![expect(
    clippy::future_not_send,
    reason = "manager ORM handles remain on their owning compio runtime"
)]

mod jobs;
mod management;

pub use jobs::{
    Admission, ClaimBatch, ClaimDeadline, ClaimReport, ClaimSkip, ClaimSkipReason, ZoneClaim,
};

use crate::{
    models::{management as management_records, management_scopes},
    queue::{lock_scope, Budget},
    Error, Queue,
};
use std::time::Duration;
use zeroship_core::app_id::AppId;
use zeroship_data_orm::orm::{Database, Entity, Filter};

/// Native queue policy; authentication and policy sources belong to the host.
#[derive(Debug, Clone, Copy)]
pub struct Options {
    pub batch_limit: usize,
    pub max_pending_management: usize,
    pub claim_budget: Duration,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            batch_limit: 128,
            max_pending_management: 1024,
            claim_budget: Duration::from_secs(5),
        }
    }
}

impl Options {
    /// # Errors
    /// Rejects empty limits or an unrepresentable claim budget.
    pub fn validate(&self) -> Result<(), Error> {
        if self.batch_limit == 0
            || self.max_pending_management == 0
            || self.max_pending_management > crate::management::MAX_PENDING_COMMANDS
            || i64::try_from(self.batch_limit).is_err()
            || i64::try_from(self.max_pending_management).is_err()
            || self.claim_budget.is_zero()
            || i64::try_from(self.claim_budget.as_millis()).is_err()
        {
            return Err(Error::Invalid);
        }
        Ok(())
    }
}

#[derive(Debug, Clone)]
pub struct Coordinator {
    queue: Queue,
    options: Options,
}

impl Coordinator {
    /// Use an already provisioned native queue.
    ///
    /// # Errors
    /// Rejects invalid policy and incompatible generated metadata.
    pub fn new(queue: Queue, options: Options) -> Result<Self, Error> {
        options.validate()?;
        queue.database.entity::<management_records::Entity>()?;
        queue.database.entity::<management_scopes::Entity>()?;
        Ok(Self { queue, options })
    }

    #[must_use]
    pub const fn queue(&self) -> &Queue {
        &self.queue
    }

    async fn scope(&self, tx: &Database, app: &AppId) -> Result<(), Error> {
        lock_scope(tx, app).await
    }

    fn budget(&self) -> Budget {
        Budget::new(self.queue.options.transaction_timeout)
    }
}

async fn count<E: Entity>(tx: &Database, filter: Filter<E>) -> Result<i64, Error> {
    match tx.entity::<E>()?.count(filter).await? {
        n if n >= 0 => Ok(n),
        _ => Err(Error::Storage),
    }
}
