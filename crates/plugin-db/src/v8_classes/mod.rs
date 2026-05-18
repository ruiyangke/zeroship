//! `#[v8_class]`-backed wrappers for the DB namespace.
//!
//! Stage 1 of the runtime-macros DB refactor (proposal
//! `docs/proposals/runtime-macros-refactor.md`). Each file in here is a
//! V8 ObjectTemplate-backed class with internal-field state and a
//! `v8::Weak` guaranteed finalizer for resource teardown:
//!
//! - [`subscription`] — `Subscription` (P8a). The headline correctness
//!   win for this stage: the broker handle is closed by the GC
//!   finalizer if user code drops the wrapper without calling
//!   `.return()` / `.close()`. Closes the P8a handle-leak that the
//!   pre-refactor handle-id-based registry could not address.
//!
//! Stage 2 — shipped:
//! - [`db`] — `Db` replaces the frozen `env.db` namespace object with
//!   a v8_class instance. Exposes `.collection(name)` returning a
//!   `Collection` v8_class. The 27 flat callbacks stay registered as
//!   own properties on this instance via `NativeRegistrar`.
//! - [`collection`] — per-collection CRUD dispatch wrapper. Each
//!   method forwards to the same-named flat callback on the parent
//!   Db with the collection name prepended (no SQL duplication).
//!
//! Stage 3 — shipped:
//! - [`transaction`] — `env.db.beginTransaction(isolationLevel?)` now
//!   resolves with a `Transaction` v8_class instance whose Weak
//!   finalizer auto-rollbacks if user code drops the handle without
//!   committing. `.commit()` / `.rollback()` are explicit methods;
//!   `.collection(name)` returns a Collection bound to the open
//!   transaction. The legacy flat `commitTransaction` /
//!   `rollbackTransaction` callbacks stay registered for back-compat
//!   with the current `@zeroship/db` SDK.
//!
//! Future stages will add:
//! - `Migration` — when the surrounding subsystem stabilises.

pub mod collection;
pub mod db;
pub mod subscription;
pub mod transaction;
