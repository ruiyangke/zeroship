//! `#[v8_class]`-backed wrappers for the DB namespace.
//!
//! Each file is a V8 ObjectTemplate-backed class with internal-field
//! state and a `v8::Weak` guaranteed finalizer for resource teardown:
//!
//! - [`db`] — `Db` backs `env.db`. Exposes `.collection(name)`
//!   returning a `Collection` v8_class wrapper.
//! - [`collection`] — per-collection CRUD. Each method walks its
//!   `v8::Local<Value>` args directly into a `serde_json::Value` via
//!   `v8_bridge::v8_value_to_serde_json` (no JSON.stringify/parse) and
//!   calls a shared `crud::dispatch_*` helper.
//! - [`transaction`] — **P9 PR 3**: hosts `mint_tx_view`, the
//!   collections-only object handed to a `Db.transaction(fn)` callback.
//!   The `Transaction` v8_class (`commit`/`rollback`/`collection` +
//!   GC-auto-rollback) is gone; transaction orchestration lives entirely
//!   in [`crate::orchestrator::transaction`]. CRUD on the view's
//!   collections routes through the open transaction connection
//!   (`IsolateDbContext::tx_conn`) automatically, since the orchestrator
//!   sets that slot for the transaction's duration.
//! - [`migrations`] — `env.db.migrations` (v8_getter) is the
//!   `Migrations` namespace exposing `.start / .status / .cancel /
//!   .reset(spec)`. `.start(spec)` returns a [`migration::Migration`]
//!   wrapper whose GC finalizer auto-cancels (transitions the audit
//!   row to `cancelled` + releases the advisory lock) if user code
//!   drops the handle without `.cancel()` / `.reset()` / a terminal
//!   `.commitBatch(isDone=true)`.
//! - [`subscription`] — `env.db.<collection>.openSubscription()` (P9 PR 1:
//!   the duplicate `env.db.openSubscription(name)` entry was removed)
//!   returns a `Subscription` wrapper. GC finalizer closes the broker
//!   entry so callers that drop the wrapper without `.close()` still release
//!   the slot.

pub mod collection;
pub mod db;
pub mod masked_value;
pub mod migration;
pub mod migrations;
pub mod replication;
pub mod subscription;
pub mod transaction;
