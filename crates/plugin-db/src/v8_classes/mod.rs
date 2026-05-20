//! `#[v8_class]`-backed wrappers for the DB namespace.
//!
//! Each file is a V8 ObjectTemplate-backed class with internal-field
//! state and a `v8::Weak` guaranteed finalizer for resource teardown:
//!
//! - [`db`] — `Db` backs `env.db`. Exposes `.collection(name)`
//!   returning a `Collection` v8_class wrapper.
//! - [`collection`] — per-collection CRUD. Each method walks its
//!   `v8::Local<Value>` args directly into a `serde_json::Value` via
//!   `callbacks::v8_value_to_serde_json` (no JSON.stringify/parse) and
//!   calls a shared `callbacks::dispatch_*` helper.
//! - [`transaction`] — `env.db.beginTransaction(isolationLevel?)`
//!   resolves with a `Transaction` instance whose Weak finalizer
//!   auto-rollbacks if user code drops the handle without explicit
//!   `.commit()` / `.rollback()`. `.collection(name)` returns a
//!   Collection wrapper bound to the open transaction (CRUD routes
//!   through `TX_CONN` automatically).
//! - [`migration`] — `env.db.migrationStart(spec)` returns a
//!   `Migration` wrapper. GC finalizer auto-cancels (transitions the
//!   audit row to `cancelled` + releases the advisory lock) if user
//!   code drops the handle without `.cancel()` / `.reset()` / a
//!   terminal `.commitBatch(isDone=true)`.
//! - [`subscription`] — `env.db.openSubscription(collection)` returns
//!   a `Subscription` wrapper. GC finalizer closes the broker entry
//!   so callers that drop the wrapper without `.close()` still release
//!   the slot.

pub mod collection;
pub mod db;
pub mod migration;
pub mod subscription;
pub mod transaction;
