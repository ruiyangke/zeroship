//! V8 wrappers for the database namespace.
//!
//! `db` and `collection` expose operations; `dispatch` captures arguments and
//! request context before delegating to the ORM. `transaction` invokes callbacks
//! under their captured transaction scope. Masked values and subscriptions own
//! native handles whose lifecycle follows their wrappers.
//!
//! Native startup owns policy finalization.

/// Exercises cold-open backend resolution and per-query unmask dispatch through
/// the crate-private V8 entry points.
#[cfg(test)]
mod cold_open;
pub mod collection;
pub mod db;
pub mod dispatch;
pub mod masked_value;
pub mod subscription;
pub mod transaction;
