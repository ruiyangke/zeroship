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
//! Future stages will add:
//! - `Db` — replaces the frozen `env.db` namespace object with a
//!   v8_class instance so `env.db.collection("users")` becomes a
//!   native method that returns a `Collection` v8_class.
//! - `Collection` — per-collection CRUD methods backed by the existing
//!   `callbacks::*` futures.
//! - `Migration`, `Transaction` — when the surrounding subsystems
//!   stabilise.

pub mod subscription;
