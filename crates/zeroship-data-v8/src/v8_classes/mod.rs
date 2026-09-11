//! V8 wrappers for the database namespace.
//!
//! `db` and `collection` expose operations; `dispatch` captures arguments and
//! request context before delegating to the ORM. `transaction` invokes callbacks
//! under their captured transaction scope. Masked values and subscriptions own
//! native handles whose lifecycle follows their wrappers.
//!
//! Internal policy installation uses the private `db_platform` capability.

/// The cold-open witness for the five V8 sites that resolve a backend, plus
/// the `find` dispatch that carries the per-query unmask hint. It is in the
/// crate rather than in an integration target because it enters through the
/// JS methods, and the three `mint_*` functions those need are `pub(crate)`.
#[cfg(test)]
mod cold_open;
pub mod collection;
pub mod db;
pub mod db_platform;
pub mod dispatch;
pub mod masked_value;
pub mod subscription;
pub mod transaction;
