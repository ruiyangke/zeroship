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
//! - [`collection`] — per-collection CRUD dispatch. Each method walks
//!   its v8::Local<Value> args directly into a `serde_json::Value` via
//!   `callbacks::v8_value_to_serde_json` (no JSON.stringify/parse
//!   boundary) and calls a shared `callbacks::dispatch_*` helper that
//!   the flat callbacks also delegate to — SQL build lives in one
//!   place.
//!
//! Stage 3 — shipped:
//! - [`migration`] — `Migration` wraps an in-flight backfill run. GC
//!   finalizer auto-cancels (transitions the audit row to `cancelled`
//!   + releases the advisory lock) if user code drops the wrapper
//!   without explicit `.cancel()` / `.reset()`. Parallel handle-leak
//!   fix to [`subscription`]. Minted by the new `migrationStart`
//!   callback; the flat `migrationBegin` / `migrationFetchBatch` /
//!   `migrationCommitBatch` callbacks stay registered for back-compat
//!   with the current `@zeroship/migrations` SDK.
//! - [`transaction`] — `env.db.beginTransaction(isolationLevel?)` now
//!   resolves with a `Transaction` v8_class instance whose Weak
//!   finalizer auto-rollbacks if user code drops the handle without
//!   committing. `.commit()` / `.rollback()` are explicit methods;
//!   `.collection(name)` returns a Collection bound to the open
//!   transaction (CRUD ops route through `TX_CONN` automatically).
//!   The legacy flat `commitTransaction` / `rollbackTransaction`
//!   callbacks stay registered for back-compat with the current
//!   `@zeroship/db` SDK; coexistence is mediated by a `TX_TOKEN`
//!   ownership tag.

pub mod collection;
pub mod db;
pub mod migration;
pub mod subscription;
pub mod transaction;
