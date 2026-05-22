//! Cross-cutting orchestrators that coordinate `crate::audit`,
//! `crate::diff`, `crate::query`, and `crate::exec` into multi-step
//! flows the V8 dispatchers expose to JS.
//!
//! Submodules:
//!
//! - [`register_model`] — the four-phase DDL pipeline behind
//!   `db.registerModel(...)`. Owns the advisory-lock guard, the
//!   diff/classify/validate/apply sequence, and the audited
//!   create-index recovery loop.
//! - [`transaction`] — explicit `db.beginTransaction()` lifecycle:
//!   mints the `Transaction` v8_class, issues BEGIN on a dedicated
//!   connection, and stamps `IsolateDbContext::tx_token` (formerly a
//!   `TX_TOKEN` thread-local, folded into `IsolateDbContext` in Stage
//!   8d-R4) so the wrapper's commit/rollback/Drop paths can fence
//!   each other.
//! - [`auto_tx`] — defense-in-depth wrappers (`__zsBeginAutoTx` /
//!   `__zsEndAutoTx`) the runtime installs on `globalThis`. Wraps
//!   `query()` / `mutation()` handlers in a per-kind isolation
//!   envelope; commits or rolls back at the end of the handler.
//!
//! Three of the four submodules (`auto_tx`, `register_model`,
//! `transaction`) are `pub` so the v8_class layer (`v8_classes/*.rs`)
//! can name their dispatch helpers; `lock_guard` is `pub(crate)`
//! since the RAII guard never crosses the crate boundary. There is
//! no aggregating re-export — `crate::callbacks` was deleted in
//! Stage 8b once each consumer moved to its canonical import.

pub mod auto_tx;
pub(crate) mod lock_guard;
pub mod register_model;
pub mod transaction;
