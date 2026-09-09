//! `#[v8_class]`-backed wrappers for the DB namespace.
//!
//! Each file is a V8 ObjectTemplate-backed class with internal-field
//! state and a `v8::Weak` guaranteed finalizer for resource teardown:
//!
//! - [`db`] — `Db` backs `env.db`. Exposes `.collection(name)`
//!   returning a `Collection` v8_class wrapper, plus the native
//!   `transaction(fn)` orchestrator. Platform-internal mask-policy and
//!   replication entry points live on [`db_platform`].
//! - [`db_platform`] — `DbPlatform`, the capability handle
//!   set on `Db` under a V8 private symbol (`ZS_PLATFORM`). Holds the
//!   platform-internal callables; unreachable from creator JS.
//! - [`collection`] — per-collection CRUD. Each method walks its
//!   `v8::Local<Value>` args directly into a `zeroship_data_query_builder::value::Value` via
//!   `v8_bridge::decode_native` (no JSON.stringify/parse) and
//!   calls a shared [`dispatch`] helper.
//! - [`dispatch`] — the 17 `dispatch_*` helpers those methods call. They
//!   lived in `crud/mod.rs` until 2026-09-02, which left `v8::` signature
//!   positions inside an ENGINE-tiered file; they are the V8 boundary, so
//!   they belong on this side of it.
//! - [`transaction`] — hosts `mint_tx_view`, the
//!   collections-only object handed to a `Db.transaction(fn)` callback.
//!   The `Transaction` v8_class (`commit`/`rollback`/`collection` +
//!   GC-auto-rollback) is gone; transaction orchestration lives entirely
//!   in [`crate::transaction`]. CRUD on the view's
//!   collections routes through the open transaction connection
//!   (`ThreadDbContext::tx_conn`) automatically, since the orchestrator
//!   sets that slot for the transaction's duration.
//! - [`subscription`] — `env.db.<collection>.openSubscription()` (the
//!   duplicate `env.db.openSubscription(name)` entry was removed)
//!   returns a `Subscription` wrapper. GC finalizer closes the broker
//!   entry so callers that drop the wrapper without `.close()` still release
//!   the slot.

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
pub mod replication;
pub mod subscription;
pub mod transaction;
