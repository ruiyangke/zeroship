//! `Migration` — native V8 wrapper for an in-flight backfill migration.
//!
//! Closes the handle-leak parallel to [`super::subscription::Subscription`]:
//! today the flat `migrationBegin` / `migrationFetchBatch` /
//! `migrationCommitBatch` / `migrationCancel` callbacks rely on the SDK
//! (`sdks/migrations/src/native.ts`) to call `migrationCancel` (or drive
//! the loop to a terminal status) on every code path — including thrown
//! errors mid-batch and the runtime tearing down a request. If user
//! code starts a migration and then drops every reference to it without
//! reaching a terminal state (e.g. an exception escapes the migrate
//! loop, the SDK's `finally` never fires), the audit row stays
//! `running`, the advisory lock leaks until the owning isolate exits,
//! and no other worker can pick the migration up.
//!
//! This wrapper owns the `(app_id, name, collection)` triple in its V8
//! internal field 0. A `v8::Weak::with_guaranteed_finalizer` registered
//! at construction spawns a best-effort `exec_cancel` (which transitions
//! the audit row to `cancelled` and lets the next worker take over) when
//! V8 collects the wrapper without `cancel()` / `reset()` having run.
//!
//! ## JS surface
//!
//! - `await env.db.migrationStart(spec)` (future: `env.db.migrations.start(spec)`)
//!   — returns a `Migration` instance once the advisory lock + audit
//!   row are claimed by this isolate.
//! - `migration.status()` → `Promise<string>` (JSON status object)
//! - `migration.cancel()` → `Promise<string>` (JSON `{"ok":true}`)
//! - `migration.reset()` → `Promise<string>` (JSON `{"ok":true}`)
//!
//! All three methods delegate to the same `exec_status` / `exec_cancel`
//! / `exec_reset` the flat callbacks use — no SQL duplication.
//!
//! The pre-refactor flat callbacks stay registered for back-compat with
//! the current `@zeroship/migrations` SDK; this v8_class is the
//! future-API path.

#![allow(unsafe_code)]

use std::cell::RefCell;
use std::rc::Rc;

use zeroship_runtime::state::OpError;
use zeroship_runtime_macros::v8_class;
#[allow(unused_imports)]
use zeroship_runtime_macros::{v8_async_method, v8_constructor, v8_method};

// ---------------------------------------------------------------------------
// Migration state
// ---------------------------------------------------------------------------

/// Owned state for one JS `Migration` instance.
///
/// Field 0 of the wrapper holds a `Box<Migration>`. The Weak finalizer
/// registered by `#[v8_class]` reclaims the Box on GC and our manual
/// `Drop` impl spawns a best-effort `exec_cancel` — so dropping the JS
/// wrapper without an explicit `.cancel()` / `.reset()` still releases
/// the advisory lock and walks the audit row to `cancelled`.
pub struct Migration {
    /// `Option` so `cancel()` / `reset()` / the GC finalizer can `take()`
    /// the active marker — subsequent operations short-circuit with a
    /// "migration not active" error, and the finalizer becomes a no-op
    /// after an explicit teardown. Mirrors the `Option`-take pattern
    /// `Subscription` uses for its `BrokerSubscription`.
    inner: RefCell<Option<MigrationOwner>>,
}

/// The identifying triple captured at `mint_migration` time.
///
/// `exec_cancel` / `exec_status` / `exec_reset` all key off
/// `(app_id, name, collection)` so we don't need to carry `audit_id` —
/// the underlying queries do `ORDER BY id DESC LIMIT 1` per row.
#[derive(Clone, Debug)]
struct MigrationOwner {
    app_id: String,
    name: String,
    collection: String,
}

impl std::fmt::Debug for Migration {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Migration")
            .field("inner", &self.inner.borrow())
            .finish()
    }
}

impl Drop for Migration {
    /// GC-time cleanup. Fires from the Weak finalizer the `#[v8_class]`
    /// macro registers on every instance: when V8 collects the wrapper
    /// object, the finalizer drops the Box, which runs this. We spawn a
    /// best-effort `exec_cancel` so the audit row transitions to
    /// `cancelled` and the session advisory lock is released — closes
    /// the handle leak parallel to `Subscription`.
    ///
    /// Idempotent: explicit `cancel()` / `reset()` take the inner state
    /// out first, and this becomes a no-op on the second pass.
    fn drop(&mut self) {
        let Some(owner) = self.inner.borrow_mut().take() else {
            return;
        };
        // V8 GC finalizers run on the V8 isolate thread which is also
        // the compio runtime thread — so `compio::runtime::spawn` is
        // safe here. We `detach` the task: the cleanup is best-effort
        // and we don't have anything to await on.
        //
        // The pool is captured by Rc-clone; if the pool was never
        // initialised (e.g. unit-test path where `DB_POOL` is never
        // populated), we silently skip the cancel call. The advisory
        // lock will still be released when the owning thread's
        // `MIG_LOCK` client is dropped on isolate teardown.
        let pool_opt: Option<Rc<compio_postgres::Pool>> =
            crate::DB_POOL.with(|p| p.borrow().as_ref().map(Rc::clone));
        let Some(pool) = pool_opt else {
            return;
        };
        compio::runtime::spawn(async move {
            // Errors are swallowed: this is GC-time best-effort and
            // there is no user-visible promise to reject. `exec_cancel`
            // returns `migration_not_cancellable` if the row already
            // reached a terminal status — fine, the user explicitly
            // drove it there.
            let _ = crate::migrations::exec_cancel(
                &pool,
                &owner.app_id,
                &owner.name,
                &owner.collection,
            )
            .await;
        })
        .detach();
    }
}

// ---------------------------------------------------------------------------
// Migration IDL surface
// ---------------------------------------------------------------------------

#[v8_class]
#[allow(dead_code)] // Methods invoked via V8 callbacks; Rust can't trace through extern.
impl Migration {
    /// Construct an empty placeholder. Real instances are minted via
    /// [`mint_migration`] — the macro requires a constructor for install
    /// codegen, so this exists to satisfy that contract. Calling
    /// `new Migration()` from JS produces a wrapper whose `inner` is
    /// `None`; every method returns the "not active" error.
    #[v8_constructor]
    fn new() -> Migration {
        Migration {
            inner: RefCell::new(None),
        }
    }

    /// `migration.status()` — read the current audit-row state. Resolves
    /// with the same JSON shape the flat `migrationStatus` callback
    /// returns (delegates to `exec_status`).
    #[v8_async_method]
    async fn status(&self) -> Result<String, OpError> {
        let owner = self
            .inner
            .borrow()
            .as_ref()
            .cloned()
            .ok_or_else(|| OpError::error("Migration: not active (already cancelled or finalised)"))?;
        let pool = ensure_pool().await?;
        crate::migrations::exec_status(&pool, &owner.app_id, &owner.name, &owner.collection)
            .await
            .map_err(OpError::error)
    }

    /// `migration.cancel()` — transition the audit row to `cancelled`.
    /// After this resolves, the next `migrationFetchBatch` on the owner
    /// thread will observe the cancellation and abort. Idempotent at
    /// the wrapper level (subsequent calls error with "not active").
    #[v8_async_method]
    async fn cancel(&self) -> Result<String, OpError> {
        // `take()` so the finalizer becomes a no-op once the user has
        // explicitly cancelled. Idempotency at the SQL layer is owned
        // by `exec_cancel` itself.
        let owner = self
            .inner
            .borrow_mut()
            .take()
            .ok_or_else(|| OpError::error("Migration: not active (already cancelled or finalised)"))?;
        let pool = ensure_pool().await?;
        crate::migrations::exec_cancel(&pool, &owner.app_id, &owner.name, &owner.collection)
            .await
            .map_err(OpError::error)
    }

    /// `migration.reset()` — clear the audit row's cursor / processed /
    /// dead-letter-pks / status back to `pending`. Same effect as the
    /// flat `migrationReset` callback. Marks the wrapper inactive so
    /// the finalizer no longer auto-cancels.
    #[v8_async_method]
    async fn reset(&self) -> Result<String, OpError> {
        let owner = self
            .inner
            .borrow_mut()
            .take()
            .ok_or_else(|| OpError::error("Migration: not active (already cancelled or finalised)"))?;
        let pool = ensure_pool().await?;
        crate::migrations::exec_reset(&pool, &owner.app_id, &owner.name, &owner.collection)
            .await
            .map_err(OpError::error)
    }
}

/// Lazy pool accessor shared by every async method on `Migration`.
/// Mirrors `callbacks::ensure_pool` but lives here so this module
/// doesn't reach into `callbacks::` private helpers.
async fn ensure_pool() -> Result<Rc<compio_postgres::Pool>, OpError> {
    let has_pool = crate::DB_POOL.with(|p| p.borrow().is_some());
    if !has_pool {
        crate::init_pool_async()
            .await
            .map_err(|e| OpError::error(format!("db: lazy init failed: {e}")))?;
    }
    crate::DB_POOL
        .with(|p| p.borrow().as_ref().map(Rc::clone))
        .ok_or_else(|| OpError::error("db: pool not initialized"))
}

// ---------------------------------------------------------------------------
// mint_migration — build a wrapper for a freshly-begun migration
// ---------------------------------------------------------------------------

/// Mint an empty `Migration` JS wrapper.
///
/// Called from [`migration_start_callback`] synchronously, BEFORE
/// `exec_begin` runs. The instance starts in an inactive state
/// (`inner = None`) so that:
///   * if `exec_begin` later succeeds, the caller flips the state to
///     active and resolves the user's promise with this wrapper;
///   * if `exec_begin` fails, the wrapper is dropped without ever
///     reaching JS — the GC finalizer sees `inner = None` and skips
///     the auto-cancel (so we don't cancel a migration this isolate
///     never owned).
///
/// Returns the JS object plus the raw `Box<Migration>` pointer
/// (laundered as `usize`) so the caller can flip the active flag from
/// the spawned future without an External-recover dance.
///
/// Mirrors `mint_subscription` exactly: Box-owned state, External in
/// internal field 0, prototype hookup, `Weak::with_guaranteed_finalizer`
/// reclaims the Box on GC.
fn mint_migration<'s>(
    scope: &mut v8::PinScope<'s, '_>,
) -> Result<(v8::Local<'s, v8::Object>, usize), OpError> {
    let class_tmpl = Migration::install(scope);
    let inst_tmpl = class_tmpl.instance_template(scope);
    let obj = inst_tmpl
        .new_instance(scope)
        .ok_or_else(|| OpError::type_error("Migration instance allocation failed"))?;

    let class_fn = class_tmpl
        .get_function(scope)
        .ok_or_else(|| OpError::type_error("Migration template missing function"))?;
    let proto_key = v8::String::new(scope, "prototype").unwrap();
    let proto_v = class_fn
        .get(scope, proto_key.into())
        .ok_or_else(|| OpError::type_error("Migration prototype missing"))?;
    obj.set_prototype(scope, proto_v);

    let state = Migration {
        inner: RefCell::new(None),
    };
    let boxed: Box<Migration> = Box::new(state);
    let raw = Box::into_raw(boxed);
    let raw_addr = raw as usize;
    let ext = v8::External::new(scope, raw as *mut std::ffi::c_void);
    obj.set_internal_field(0, ext.into());

    // SAFETY: `raw_addr` was Box::into_raw'd from `Box<Migration>`; the
    // finalizer closure casts back to the same type and drops the Box
    // exactly once when V8 reclaims the wrapper. `Migration`'s `Drop`
    // impl spawns the best-effort cancel if `inner` is still `Some`.
    let weak = v8::Weak::with_guaranteed_finalizer(
        scope,
        obj,
        Box::new(move || unsafe {
            drop(Box::from_raw(raw_addr as *mut Migration));
        }),
    );
    std::mem::forget(weak);

    Ok((obj, raw_addr))
}

// ---------------------------------------------------------------------------
// migration_start_callback — `env.db.migrationStart(spec)` entry point
// ---------------------------------------------------------------------------

/// V8 callback for `zeroship.db.migrationStart(spec)`.
///
/// Mints a `Migration` wrapper, returns a Promise that resolves to it
/// once the underlying `exec_begin` has acquired the advisory lock and
/// written the audit row. Future-API counterpart to the flat
/// `migrationBegin` / `migrationFetchBatch` / `migrationCommitBatch`
/// callbacks the current `@zeroship/migrations` SDK uses.
///
/// `spec` is a JS object: `{ name, collection, batchSize?, dryRun? }`.
/// `batchSize` is accepted for forward-compat with the future SDK
/// shape; this callback itself doesn't use it (it belongs to
/// `migrationFetchBatch`).
///
/// Registered in `lib.rs` under the name `migrationStart`; the
/// callbacks.rs side is a thin shim so the `unsafe` activation hop
/// after the await stays in this module (which opts in to
/// `#![allow(unsafe_code)]`).
pub fn migration_start_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    use zeroship_runtime::state::{OpResult, ResolveValue, SharedState};

    let state: SharedState = scope
        .get_slot::<SharedState>()
        .expect("RuntimeState not in isolate slot")
        .clone();

    // ---- 1. Parse spec ---------------------------------------------------
    // The spec arrives as a JS object; serialise via JSON to a serde
    // Value so we can pull `name` / `collection` / `dryRun` / `reset`
    // out by key without bespoke `v8::Object::get` plumbing per field.
    let spec_val = args.get(0);
    if spec_val.is_null_or_undefined() {
        throw_type_error(scope, "db: migrationStart: spec must be an object");
        return;
    }
    let spec = match parse_spec(scope, spec_val) {
        Ok(s) => s,
        Err(msg) => {
            throw_type_error(scope, &msg);
            return;
        }
    };

    // ---- 2. Mint the (inactive) Migration wrapper synchronously --------
    // If exec_begin fails later, this wrapper is unreachable from JS
    // (the user's promise rejects) — the GC finalizer sees `inner =
    // None` and skips the auto-cancel.
    let (migration_obj, raw_addr) = match mint_migration(scope) {
        Ok(pair) => pair,
        Err(e) => {
            throw_error(scope, &e.message);
            return;
        }
    };
    // Upcast Object -> Value so we can hand it to `ResolveValue::JsGlobal`.
    let migration_val: v8::Local<v8::Value> = migration_obj.into();
    let migration_global: v8::Global<v8::Value> = v8::Global::new(scope, migration_val);

    // ---- 3. Allocate the user-facing Promise ---------------------------
    let resolver = v8::PromiseResolver::new(scope).unwrap();
    let promise = resolver.get_promise(scope);
    let resolver_global = v8::Global::new(scope, resolver);

    let request_id = state.borrow().executing_request_id;
    let app_id = crate::callbacks::get_app_id_pub(&state);

    // ---- 4. Spawn the async exec_begin op ------------------------------
    let SpecParts { name, collection, dry_run, reset } = spec;
    state.borrow_mut().spawned_ops.push(Box::pin(async move {
        // The `migration_global` capture keeps the wrapper Object alive
        // for the entire future — so the Box behind `raw_addr` cannot
        // be GC'd before we either activate (success) or drop the
        // Global (failure).
        let pool = match ensure_pool().await {
            Ok(p) => p,
            Err(e) => {
                drop(migration_global);
                return OpResult::JsValue {
                    resolver: resolver_global,
                    value: ResolveValue::RejectError(e),
                    request_id,
                };
            }
        };
        match crate::migrations::exec_begin(
            &pool, &app_id, &name, &collection, dry_run, reset,
        )
        .await
        {
            Ok(_begin_json) => {
                // SAFETY: `raw_addr` came from `mint_migration` above
                // in this same op. `migration_global` is still live in
                // this future capture, which pins the wrapper Object —
                // so the Box behind `raw_addr` has not been GC'd. We
                // flip `inner = Some(owner)` before resolving the
                // promise, so the wrapper reaches JS in the active
                // state. The macro rejects `&mut self` async methods,
                // so we only take a `&Migration` here (RefCell-guarded
                // interior mutation).
                let migration: &Migration =
                    unsafe { &*(raw_addr as *const Migration) };
                *migration.inner.borrow_mut() = Some(MigrationOwner {
                    app_id,
                    name,
                    collection,
                });
                OpResult::JsValue {
                    resolver: resolver_global,
                    value: ResolveValue::JsGlobal(migration_global),
                    request_id,
                }
            }
            Err(e) => {
                // exec_begin never wrote our audit row (or didn't take
                // the lock). Dropping the Global is the only reference;
                // wrapper becomes GC eligible; finalizer no-ops since
                // `inner` is still `None`.
                drop(migration_global);
                OpResult::JsValue {
                    resolver: resolver_global,
                    value: ResolveValue::RejectError(OpError::error(e)),
                    request_id,
                }
            }
        }
    }));
    // Wake the pump so the begin op starts promptly.
    let notify = state.borrow().pump_notify_tx.clone();
    if let Some(mut tx) = notify {
        let _ = tx.try_send(());
    }
    rv.set(promise.into());
}

// ---------------------------------------------------------------------------
// Spec parsing helpers
// ---------------------------------------------------------------------------

struct SpecParts {
    name: String,
    collection: String,
    dry_run: bool,
    reset: bool,
}

/// Parse the `spec` object into typed fields. Returns a human-readable
/// error message on validation failure so the caller throws a single
/// `TypeError`.
fn parse_spec(
    scope: &mut v8::PinScope<'_, '_>,
    spec: v8::Local<v8::Value>,
) -> Result<SpecParts, String> {
    if !spec.is_object() {
        return Err("db: migrationStart: spec must be an object".into());
    }
    let json_str = v8::json::stringify(scope, spec)
        .ok_or_else(|| "db: migrationStart: failed to serialise spec".to_string())?
        .to_rust_string_lossy(scope);
    let parsed: serde_json::Value = serde_json::from_str(&json_str)
        .map_err(|e| format!("db: migrationStart: invalid spec JSON: {e}"))?;
    let obj = parsed
        .as_object()
        .ok_or_else(|| "db: migrationStart: spec must be an object".to_string())?;
    let name = obj
        .get("name")
        .and_then(serde_json::Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "db: migrationStart: spec.name must be a non-empty string".to_string())?
        .to_string();
    let collection = obj
        .get("collection")
        .and_then(serde_json::Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            "db: migrationStart: spec.collection must be a non-empty string".to_string()
        })?
        .to_string();
    let dry_run = obj
        .get("dryRun")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    let reset = obj
        .get("reset")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    Ok(SpecParts {
        name,
        collection,
        dry_run,
        reset,
    })
}

fn throw_type_error(scope: &mut v8::PinScope<'_, '_>, msg: &str) {
    let m = v8::String::new(scope, msg).unwrap();
    let exc = v8::Exception::type_error(scope, m);
    scope.throw_exception(exc);
}

fn throw_error(scope: &mut v8::PinScope<'_, '_>, msg: &str) {
    let m = v8::String::new(scope, msg).unwrap();
    let exc = v8::Exception::error(scope, m);
    scope.throw_exception(exc);
}
