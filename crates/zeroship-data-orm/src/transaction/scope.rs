//! Identity carried by a transaction callback and its continuations.

use crate::binding::DbRoute;
use crate::error::DbError;

/// A captured transaction and savepoint identity. A route's next transaction
/// must never inherit work left running by a previous callback.
#[derive(Clone, Debug)]
pub struct TransactionScope {
    route: DbRoute,
    generation: u64,
    frame: u64,
}

impl TransactionScope {
    /// Decode the scope preserved by a host's async context. These values are
    /// observations, checked against the live ORM protocol before use.
    pub fn observed(route: DbRoute, generation: u64, frame: u64) -> Self {
        Self {
            route,
            generation,
            frame,
        }
    }

    /// Capture the frame after BEGIN or SAVEPOINT has succeeded.
    pub fn current(route: &DbRoute) -> Result<Self, DbError> {
        crate::tx_lanes::with(|lanes| {
            let reducer = lanes.transaction_reducer(route).ok_or_else(expired)?;
            let generation = reducer.generation().ok_or_else(expired)?;
            let frame = reducer.frames().top().ok_or_else(expired)?.id();
            Ok(Self::observed(route.clone(), generation.0, frame.get()))
        })
    }

    /// The tenant AND database this scope belongs to.
    pub fn route(&self) -> &DbRoute {
        &self.route
    }

    pub fn app_id(&self) -> &str {
        self.route.app_id()
    }
    pub fn generation(&self) -> u64 {
        self.generation
    }
    pub fn frame(&self) -> u64 {
        self.frame
    }

    /// Check immediately before admitting SQL, including after async backend
    /// resolution. An open child also excludes work from its parent frame.
    pub fn check(&self) -> Result<(), DbError> {
        crate::tx_lanes::with(|lanes| {
            let reducer = lanes.transaction_reducer(&self.route).ok_or_else(expired)?;
            if reducer.generation().map(|generation| generation.0) != Some(self.generation)
                || !reducer.frames().contains(self.frame)
            {
                return Err(expired());
            }
            if reducer.frames().top().map(|frame| frame.id().get()) != Some(self.frame) {
                return Err(DbError::validation_hinted(
                    "transaction_connection_busy",
                    "db: a nested transaction is using this transaction's connection".to_owned(),
                    "Await the nested transaction before issuing more work in its parent callback.",
                ));
            }
            Ok(())
        })
    }
}

pub(crate) fn expired() -> DbError {
    DbError::validation_hinted(
        "transaction_scope_expired",
        "db: this operation belongs to a transaction scope that has already settled".to_owned(),
        "Await every database call started inside a transaction before its callback returns.",
    )
}
