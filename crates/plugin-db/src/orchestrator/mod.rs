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
//!   connection, and stamps `TX_TOKEN` so the wrapper's
//!   commit/rollback/Drop paths can fence each other.
//! - [`auto_tx`] — defense-in-depth wrappers (`__zsBeginAutoTx` /
//!   `__zsEndAutoTx`) the runtime installs on `globalThis`. Wraps
//!   `query()` / `mutation()` handlers in a per-kind isolation
//!   envelope; commits or rolls back at the end of the handler.
//!
//! Each submodule is `pub(crate)` so [`crate::callbacks`] can
//! re-export the public symbols at the legacy path.

pub(crate) mod register_model;
