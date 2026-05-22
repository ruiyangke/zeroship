//! `Migration` — native V8 wrapper for an in-flight backfill migration.
//!
//! The wrapper owns the `(app_id, name, collection)` triple in its V8
//! internal field 0. A `v8::Weak::with_guaranteed_finalizer` registered
//! at construction spawns a best-effort `exec_cancel` (which transitions
//! the audit row to `cancelled` and lets the next worker take over) when
//! V8 collects the wrapper without `cancel()` / `reset()` / a terminal
//! `commitBatch(isDone=true)` having run — so a thrown error escaping
//! the SDK's batch loop doesn't leak the advisory lock or strand the
//! audit row in `running`.
//!
//! ## JS surface
//!
//! - `await env.db.migrations.start(spec)` — mints a `Migration` instance
//!   once the advisory lock + audit row are claimed by this isolate.
//! - `m.fetchBatch(cursor, batchSize)` — read the next batch
//! - `m.commitBatch(updates, deadLetterPks, nextCursor, processed,
//!    isDone, terminalStatus, errorMessage)` — commit one batch
//! - `m.status()` — `Promise<string>` (JSON status object)
//! - `m.cancel()` — `Promise<string>` (JSON `{"ok":true}`)
//! - `m.reset()` — `Promise<string>` (JSON `{"ok":true}`)
//!
//! Methods delegate to `crate::migrations::exec_*` so all SQL lives in
//! one place. The sibling `env.db.migrations.status` /
//! `env.db.migrations.cancel` / `env.db.migrations.reset` methods cover
//! the observe-by-name path (no advisory lock).

#![allow(unsafe_code)]

use std::cell::RefCell;

use zeroship_runtime::state::{JsonValue, OpError};
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
    /// Coordinates remembered from `mint_migration` so `status()`
    /// remains readable after `cancel()` / `reset()` / a terminal
    /// `commitBatch(isDone=true)` has cleared `inner`. The audit row
    /// outlives the wrapper's active phase; `status()` reads it by
    /// `(name, collection)` without the advisory lock.
    coords: RefCell<Option<MigrationOwner>>,
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
        // The backend handle is captured by Rc-clone; if the backend
        // was never initialised (e.g. unit-test path where the per-
        // isolate context's pool slot is empty), we silently skip
        // the cancel call. The advisory lock will still be released
        // when the owning thread's migration lock client is dropped
        // on isolate teardown.
        let backend_opt = crate::context::with(|c| c.backend());
        let Some(backend) = backend_opt else {
            return;
        };
        // `compio::runtime::spawn` panics if the runtime is mid-
        // shutdown (e.g. isolate teardown triggered this finalizer
        // pass). A Rust panic from a V8 finalizer crosses an
        // `extern "C"` boundary and aborts the worker; this is
        // best-effort cleanup so we swallow the panic and let the
        // advisory lock release with the dying connection.
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            compio::runtime::spawn(async move {
                // P0 PR 5: `BackendHandle` enum (round-3 CRITICAL #3
                // closure). Unwrap the PG arm via `as_postgres` so
                // the trailing `.await` doesn't have to live inside
                // a `with_postgres` closure.
                let Some(pg) = backend.as_postgres() else {
                    return;
                };
                let _ = crate::migrations::exec_cancel(
                    pg,
                    &owner.app_id,
                    &owner.name,
                    &owner.collection,
                )
                .await;
            })
            .detach();
        }));
    }
}

// ---------------------------------------------------------------------------
// Migration IDL surface
// ---------------------------------------------------------------------------

#[v8_class]
#[allow(dead_code)] // Methods invoked via V8 callbacks; Rust can't trace through extern.
impl Migration {
    /// `new Migration()` from JS rejects — real instances are minted
    /// via [`mint_migration`] from `env.db.migrations.start(spec)`,
    /// which stamps the `(app_id, name, collection)` triple onto the
    /// wrapper.
    #[v8_constructor]
    fn new() -> Result<Migration, OpError> {
        Err(OpError::type_error("Illegal constructor"))
    }

    /// `migration.status()` — read the current audit-row state.
    /// Resolves with the typed status object (mirrors the SDK's
    /// `NativeStatus` interface). Safe to call after `cancel()` /
    /// `reset()` / a terminal `commitBatch(isDone=true)` — the
    /// wrapper retains the `(name, collection)` coordinates so the
    /// audit row remains observable.
    #[v8_async_method]
    async fn status(&self) -> Result<JsonValue, OpError> {
        let owner = self
            .coords
            .borrow()
            .as_ref()
            .cloned()
            .ok_or_else(|| {
                crate::error::DbError::validation(
                    "migration_not_initialised",
                    "Migration: not initialised",
                )
                .to_op_error()
            })?;
        let backend = ensure_backend().await?;
        let pg = backend
            .as_postgres()
            .expect("PostgresBackend arm — migration RPC is PG-only in P0");
        crate::migrations::exec_status(pg, &owner.app_id, &owner.name, &owner.collection)
            .await
            .map(JsonValue)
    }

    /// `migration.cancel()` — transition the audit row to `cancelled`.
    /// After this resolves, the next `fetchBatch()` on the owner thread
    /// will observe the cancellation and abort. Idempotent at the
    /// wrapper level (subsequent calls error with "not active").
    #[v8_async_method]
    async fn cancel(&self) -> Result<(), OpError> {
        // `take()` so the finalizer becomes a no-op once the user has
        // explicitly cancelled. Idempotency at the SQL layer is owned
        // by `exec_cancel` itself.
        let owner = self
            .inner
            .borrow_mut()
            .take()
            .ok_or_else(crate::migrations::err_not_active)?;
        let backend = ensure_backend().await?;
        let pg = backend
            .as_postgres()
            .expect("PostgresBackend arm — migration RPC is PG-only in P0");
        crate::migrations::exec_cancel(pg, &owner.app_id, &owner.name, &owner.collection)
            .await
            .map(|_| ())
    }

    /// `migration.reset()` — clear the audit row's cursor / processed /
    /// dead-letter-pks / status back to `pending`. Same effect as
    /// `env.db.migrations.reset({name, collection})`. Marks the wrapper
    /// inactive so the finalizer no longer auto-cancels.
    #[v8_async_method]
    async fn reset(&self) -> Result<(), OpError> {
        let owner = self
            .inner
            .borrow_mut()
            .take()
            .ok_or_else(crate::migrations::err_not_active)?;
        let backend = ensure_backend().await?;
        let pg = backend
            .as_postgres()
            .expect("PostgresBackend arm — migration RPC is PG-only in P0");
        crate::migrations::exec_reset(pg, &owner.app_id, &owner.name, &owner.collection)
            .await
            .map(|_| ())
    }

    /// `migration.fetchBatch(cursor, batchSize)` — read the next batch
    /// of rows after `cursor`. Resolves with the row array directly.
    /// Delegates to `exec_fetch_batch` (which itself checks
    /// [`crate::context::IsolateDbContext::mig_lock`] for ownership /
    /// cancellation).
    #[v8_async_method]
    #[v8_name = "fetchBatch"]
    async fn fetch_batch(&self, cursor: f64, batch_size: f64) -> Result<JsonValue, OpError> {
        let owner = self
            .inner
            .borrow()
            .as_ref()
            .cloned()
            .ok_or_else(crate::migrations::err_not_active)?;
        let cursor_i = checked_int(cursor, "cursor")?;
        let batch_size_i = checked_int(batch_size, "batchSize")?;
        // P0 PR 4: `exec_fetch_batch` no longer needs the backend —
        // every operation runs on the lock client parked in the
        // per-isolate context.
        crate::migrations::exec_fetch_batch(&owner.app_id, cursor_i, batch_size_i)
            .await
            .map(JsonValue)
    }

    /// `migration.commitBatch(spec)` — apply one batch of per-row
    /// updates, advance the cursor, optionally finalise the run.
    ///
    /// `spec` is `{ updates: Array<{id, set}>, deadLetterPks: number[],
    /// nextCursor: number, processedTotal: number, isDone: boolean,
    /// terminalStatus?: string, errorMessage?: string }` — walked
    /// directly from V8 so the SDK loop never JSON-stringifies the per-
    /// row payload. Resolves void on success; rejects with the Postgres
    /// error on failure. On `isDone=true`, clears the wrapper's inner
    /// state so subsequent calls + the finalizer no-op.
    #[v8_method]
    #[v8_name = "commitBatch"]
    fn commit_batch<'s>(
        &self,
        scope: &mut v8::PinScope<'s, '_>,
        wrapper: v8::Local<v8::Object>,
        spec_val: v8::Local<v8::Value>,
    ) -> v8::Local<'s, v8::Value> {
        commit_batch_with_spec(scope, self, wrapper, spec_val).into()
    }
}

/// Lazy backend accessor — initialises the pool via
/// [`crate::exec::ensure_pool`] (which constructs the
/// [`crate::backend::PostgresBackend`] alongside the pool in
/// `IsolateDbContext::set_pool`) and then returns the per-isolate
/// [`crate::backend::BackendHandle`] enum. The `Migration`
/// v8_async_methods route every migration RPC through this handle so
/// they stay free of direct `compio_postgres` type references.
///
/// **P0 PR 5**: returns the typed enum instead of an
/// `Rc<PostgresBackend>` (round-3 critic CRITICAL #3 closure). Call
/// sites unwrap the PG arm via [`crate::backend::BackendHandle::as_postgres`]
/// — the async paths can't compose a closure-based `with_postgres`
/// across the trailing `.await`, so the borrowed-reference shape
/// wins out.
async fn ensure_backend() -> Result<crate::backend::BackendHandle, OpError> {
    crate::exec::ensure_pool()
        .await
        .map_err(crate::error::DbError::to_op_error)?;
    crate::context::with(|c| c.backend()).ok_or_else(|| {
        crate::error::DbError::config("backend_not_initialized", "db: backend not initialized")
            .to_op_error()
    })
}

/// Coerce a JS number to a finite, integer-valued `i64`. Used by
/// `fetchBatch` / `commitBatch` for `cursor` / `nextCursor` /
/// `processedTotal` / `batchSize` where silent `as i64` truncation
/// would corrupt huge migration runs.
fn checked_int(value: f64, field: &str) -> Result<i64, OpError> {
    if !value.is_finite() || value.fract() != 0.0 {
        return Err(OpError::type_error(format!(
            "db: commitBatch: {field} must be a finite integer (got {value})"
        )));
    }
    // `i64::MAX as f64` rounds UP to 9223372036854775808.0 (= 2^63, one
    // past i64::MAX), so a value of exactly 2^63 would pass a `>` test
    // and then saturate to i64::MAX under `as i64`. Use `>=` against
    // 2^63 to reject it instead. `i64::MIN` (-2^63) is exactly
    // representable, so `<` against it is the right bound below.
    #[allow(clippy::cast_precision_loss)]
    if value < i64::MIN as f64 || value >= 9_223_372_036_854_775_808.0_f64 {
        return Err(OpError::range_error(format!(
            "db: commitBatch: {field} out of i64 range (got {value})"
        )));
    }
    #[allow(clippy::cast_possible_truncation)]
    Ok(value as i64)
}

// ---------------------------------------------------------------------------
// commit_batch_with_spec — body of `Migration.commitBatch(spec)`
// ---------------------------------------------------------------------------

/// Parsed shape of the `spec` object handed to `commitBatch`.
struct CommitSpec {
    updates: serde_json::Value,
    dead_letter_pks: serde_json::Value,
    next_cursor: i64,
    processed_total: i64,
    is_done: bool,
    terminal_status: Option<String>,
    error_message: Option<String>,
}

/// Synchronously parse the spec, mint a Promise, and spawn the
/// `exec_commit_batch` future. Resolves with `undefined`; rejects on
/// Postgres error.
///
/// `wrapper` is the JS Migration object (`args.this()`); we capture a
/// `v8::Global` of it into the async block so V8 can't finalise the
/// Box<Migration> behind `this` while the future is alive. Sync
/// `#[v8_method]`s don't auto-pin their receiver (only
/// `#[v8_async_method]` does — see `runtime-macros/.../method.rs`), so
/// without this Global a JS-side `m.commitBatch(spec)` whose only
/// reference is dropped before the spawned op settles would let GC
/// reclaim the wrapper mid-await; the post-`exec_commit_batch` `&*` of
/// `this_addr` would then be a use-after-free.
fn commit_batch_with_spec<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    this: &Migration,
    wrapper: v8::Local<v8::Object>,
    spec_val: v8::Local<v8::Value>,
) -> v8::Local<'s, v8::Promise> {
    use zeroship_runtime::state::{OpResult, ResolveValue, SharedState};

    let state: SharedState = scope
        .get_slot::<SharedState>()
        .expect("RuntimeState not in isolate slot")
        .clone();

    let resolver = v8::PromiseResolver::new(scope).unwrap();
    let promise = resolver.get_promise(scope);

    let owner = match this.inner.borrow().as_ref().cloned() {
        Some(o) => o,
        None => {
            // Build the same coded error here that the async methods
            // surface via `OpError::coded` — so the SDK sees the same
            // `e.code = "migration_not_active"` regardless of which
            // entry point caught the inactive wrapper.
            let err = crate::migrations::err_not_active();
            let m = v8::String::new(scope, &err.message).unwrap();
            let exc = v8::Exception::error(scope, m);
            if let Ok(obj) = v8::Local::<v8::Object>::try_from(exc) {
                if let zeroship_runtime::state::OpErrorKind::CodedError { code, hint } = &err.kind {
                    let ck = v8::String::new(scope, "code").unwrap();
                    let cv = v8::String::new(scope, code).unwrap();
                    obj.set(scope, ck.into(), cv.into());
                    if let Some(h) = hint {
                        let hk = v8::String::new(scope, "hint").unwrap();
                        let hv = v8::String::new(scope, h).unwrap();
                        obj.set(scope, hk.into(), hv.into());
                    }
                }
            }
            resolver.reject(scope, exc);
            return promise;
        }
    };

    let spec = match parse_commit_spec(scope, spec_val) {
        Ok(s) => s,
        Err(msg) => {
            let m = v8::String::new(scope, &msg).unwrap();
            let exc = v8::Exception::type_error(scope, m);
            resolver.reject(scope, exc);
            return promise;
        }
    };

    let resolver_global = v8::Global::new(scope, resolver);
    let request_id = state.borrow().executing_request_id;
    let is_done = spec.is_done;

    // Pin the JS wrapper across the await so the Box<Migration> behind
    // `this_addr` stays live. `#[v8_method]` (sync) doesn't pin the
    // receiver — only `#[v8_async_method]` does. The raw pointer is a
    // typed shortcut into the Box; the Global is what keeps the Box
    // alive.
    let wrapper_global = v8::Global::new(scope, wrapper);
    let this_ptr: *const Migration = this;
    let this_addr = this_ptr as usize;

    state.borrow_mut().spawned_ops.push(Box::pin(async move {
        let _keepalive = wrapper_global;
        let backend = match ensure_backend().await {
            Ok(b) => b,
            Err(e) => {
                return OpResult::JsValue {
                    resolver: resolver_global,
                    value: ResolveValue::RejectError(e),
                    request_id,
                };
            }
        };
        let pg = backend
            .as_postgres()
            .expect("PostgresBackend arm — commitBatch is PG-only in P0");
        let terminal_status_opt = spec.terminal_status.as_deref();
        let error_message_opt = spec.error_message.as_deref();
        let result = crate::migrations::exec_commit_batch(
            pg,
            &owner.app_id,
            &spec.updates,
            &spec.dead_letter_pks,
            spec.next_cursor,
            spec.processed_total,
            is_done,
            terminal_status_opt,
            error_message_opt,
        )
        .await;
        match result {
            Ok(_) => {
                if is_done {
                    // SAFETY: `_keepalive` (the wrapper Global captured
                    // above) is still in scope here, pinning the Box
                    // behind `this_addr`. Clearing `inner` is a RefCell
                    // mutation; the macro rejects `&mut self` async, so
                    // no aliasing risk from a concurrent &mut.
                    let inst: &Migration = unsafe { &*(this_addr as *const Migration) };
                    *inst.inner.borrow_mut() = None;
                }
                OpResult::JsValue {
                    resolver: resolver_global,
                    value: ResolveValue::Undefined,
                    request_id,
                }
            }
            Err(e) => OpResult::JsValue {
                resolver: resolver_global,
                value: ResolveValue::RejectError(e),
                request_id,
            },
        }
    }));

    let notify = state.borrow().pump_notify_tx.clone();
    if let Some(mut tx) = notify {
        let _ = tx.try_send(());
    }

    promise
}

fn parse_commit_spec(
    scope: &mut v8::PinScope<'_, '_>,
    spec_val: v8::Local<v8::Value>,
) -> Result<CommitSpec, String> {
    if !spec_val.is_object() {
        return Err("db: commitBatch: spec must be an object".into());
    }
    let parsed = crate::v8_bridge::v8_value_to_serde_json(scope, spec_val);
    let obj = parsed
        .as_object()
        .ok_or_else(|| "db: commitBatch: spec must be an object".to_string())?;

    let updates = obj
        .get("updates")
        .cloned()
        .unwrap_or(serde_json::Value::Array(Vec::new()));
    if !updates.is_array() {
        return Err("db: commitBatch: spec.updates must be an array".into());
    }

    let dead_letter_pks = obj
        .get("deadLetterPks")
        .cloned()
        .unwrap_or(serde_json::Value::Array(Vec::new()));
    if !dead_letter_pks.is_array() {
        return Err("db: commitBatch: spec.deadLetterPks must be an array".into());
    }

    let next_cursor_n = obj
        .get("nextCursor")
        .and_then(serde_json::Value::as_f64)
        .ok_or_else(|| "db: commitBatch: spec.nextCursor must be a number".to_string())?;
    let next_cursor = checked_int(next_cursor_n, "nextCursor")
        .map_err(|e| e.message)?;

    let processed_total_n = obj
        .get("processedTotal")
        .and_then(serde_json::Value::as_f64)
        .ok_or_else(|| "db: commitBatch: spec.processedTotal must be a number".to_string())?;
    let processed_total = checked_int(processed_total_n, "processedTotal")
        .map_err(|e| e.message)?;

    let is_done = obj
        .get("isDone")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);

    let terminal_status = obj
        .get("terminalStatus")
        .and_then(serde_json::Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let error_message = obj
        .get("errorMessage")
        .and_then(serde_json::Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_string);

    Ok(CommitSpec {
        updates,
        dead_letter_pks,
        next_cursor,
        processed_total,
        is_done,
        terminal_status,
        error_message,
    })
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
        coords: RefCell::new(None),
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
// migration_start_with_spec — body of `env.db.migrations.start(spec)`
// ---------------------------------------------------------------------------

/// Synchronously parses `spec`, mints the (inactive) `Migration`
/// wrapper, and returns the user-facing Promise; resolution /
/// rejection happens inside the spawned `exec_begin` op. Called from
/// `Migrations::start` (the `#[v8_method]` in
/// [`super::migrations`]).
///
/// `spec` is a JS object: `{ name, collection, dryRun?, reset? }`.
pub(crate) fn migration_start_with_spec<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    spec_val: v8::Local<v8::Value>,
) -> v8::Local<'s, v8::Promise> {
    use zeroship_runtime::state::{OpResult, ResolveValue, SharedState};

    let state: SharedState = scope
        .get_slot::<SharedState>()
        .expect("RuntimeState not in isolate slot")
        .clone();

    // ---- 1. Parse spec ---------------------------------------------------
    // The spec arrives as a JS object; serialise via JSON to a serde
    // Value so we can pull `name` / `collection` / `dryRun` / `reset`
    // out by key without bespoke `v8::Object::get` plumbing per field.
    let resolver = v8::PromiseResolver::new(scope).unwrap();
    let early_promise = resolver.get_promise(scope);
    if spec_val.is_null_or_undefined() {
        let m = v8::String::new(scope, "db: migrations.start: spec must be an object").unwrap();
        let exc = v8::Exception::type_error(scope, m);
        resolver.reject(scope, exc);
        return early_promise;
    }
    let spec = match parse_spec(scope, spec_val) {
        Ok(s) => s,
        Err(msg) => {
            let m = v8::String::new(scope, &msg).unwrap();
            let exc = v8::Exception::type_error(scope, m);
            resolver.reject(scope, exc);
            return early_promise;
        }
    };

    // ---- 2. Mint the (inactive) Migration wrapper synchronously --------
    // If exec_begin fails later, this wrapper is unreachable from JS
    // (the user's promise rejects) — the GC finalizer sees `inner =
    // None` and skips the auto-cancel.
    let (migration_obj, raw_addr) = match mint_migration(scope) {
        Ok(pair) => pair,
        Err(e) => {
            let m = v8::String::new(scope, &e.message).unwrap();
            let exc = v8::Exception::error(scope, m);
            resolver.reject(scope, exc);
            return early_promise;
        }
    };
    // Upcast Object -> Value so we can hand it to `ResolveValue::JsGlobal`.
    let migration_val: v8::Local<v8::Value> = migration_obj.into();
    let migration_global: v8::Global<v8::Value> = v8::Global::new(scope, migration_val);

    // ---- 3. Use the user-facing Promise ---------------------------
    let promise = resolver.get_promise(scope);
    let resolver_global = v8::Global::new(scope, resolver);

    let request_id = state.borrow().executing_request_id;
    let app_id = crate::v8_bridge::get_app_id_pub(&state);

    // ---- 4. Spawn the async exec_begin op ------------------------------
    let SpecParts { name, collection, dry_run, reset } = spec;
    state.borrow_mut().spawned_ops.push(Box::pin(async move {
        // The `migration_global` capture keeps the wrapper Object alive
        // for the entire future — so the Box behind `raw_addr` cannot
        // be GC'd before we either activate (success) or drop the
        // Global (failure).
        let backend = match ensure_backend().await {
            Ok(b) => b,
            Err(e) => {
                drop(migration_global);
                return OpResult::JsValue {
                    resolver: resolver_global,
                    value: ResolveValue::RejectError(e),
                    request_id,
                };
            }
        };
        let pg = backend
            .as_postgres()
            .expect("PostgresBackend arm — migration.start is PG-only in P0");
        match crate::migrations::exec_begin(
            pg, &app_id, &name, &collection, dry_run, reset,
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
                let owner = MigrationOwner {
                    app_id,
                    name,
                    collection,
                };
                *migration.inner.borrow_mut() = Some(owner.clone());
                *migration.coords.borrow_mut() = Some(owner);
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
                    value: ResolveValue::RejectError(e),
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
    promise
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
        return Err("db: migrations.start: spec must be an object".into());
    }
    // Walk the V8 object directly into a serde_json::Value (no
    // JSON.stringify / JSON.parse boundary). Same walker the
    // Collection CRUD methods use — keeps the migration entry point
    // consistent with the rest of the native surface.
    let parsed = crate::v8_bridge::v8_value_to_serde_json(scope, spec);
    let obj = parsed
        .as_object()
        .ok_or_else(|| "db: migrations.start: spec must be an object".to_string())?;
    let name = obj
        .get("name")
        .and_then(serde_json::Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| "db: migrations.start: spec.name must be a non-empty string".to_string())?
        .to_string();
    let collection = obj
        .get("collection")
        .and_then(serde_json::Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| {
            "db: migrations.start: spec.collection must be a non-empty string".to_string()
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

#[cfg(test)]
mod tests {
    use super::checked_int;

    #[test]
    fn checked_int_accepts_i64_max() {
        // `i64::MAX` is NOT exactly representable as f64 — the nearest
        // f64 below is the value JS would actually produce, and it
        // converts back to a positive i64 strictly less than i64::MAX.
        let ok = checked_int(9_223_372_036_854_775_000.0, "x").unwrap();
        assert!(ok > 0 && ok <= i64::MAX);
    }

    #[test]
    fn checked_int_rejects_2pow63() {
        // 2^63 == 9223372036854775808.0 is one past i64::MAX. Pre-fix
        // this slipped through `> i64::MAX as f64` (since the cast
        // rounds UP to the same value) and saturated under `as i64`.
        let v = 9_223_372_036_854_775_808.0_f64;
        let err = checked_int(v, "x").unwrap_err();
        assert!(err.message.contains("out of i64 range"), "msg: {}", err.message);
    }

    #[test]
    fn checked_int_accepts_i64_min() {
        let ok = checked_int(i64::MIN as f64, "x").unwrap();
        assert_eq!(ok, i64::MIN);
    }

    #[test]
    fn checked_int_rejects_below_min() {
        // One ULP below i64::MIN (i.e. more negative).
        let v = (i64::MIN as f64) * 2.0;
        let err = checked_int(v, "x").unwrap_err();
        assert!(err.message.contains("out of i64 range"));
    }

    #[test]
    fn checked_int_rejects_nan() {
        let err = checked_int(f64::NAN, "x").unwrap_err();
        assert!(err.message.contains("finite integer"));
    }

    #[test]
    fn checked_int_rejects_infinity() {
        let err = checked_int(f64::INFINITY, "x").unwrap_err();
        assert!(err.message.contains("finite integer"));
        let err = checked_int(f64::NEG_INFINITY, "x").unwrap_err();
        assert!(err.message.contains("finite integer"));
    }

    #[test]
    fn checked_int_rejects_fractional() {
        let err = checked_int(1.5, "x").unwrap_err();
        assert!(err.message.contains("finite integer"));
    }

    #[test]
    fn checked_int_accepts_zero_and_small() {
        assert_eq!(checked_int(0.0, "x").unwrap(), 0);
        assert_eq!(checked_int(1.0, "x").unwrap(), 1);
        assert_eq!(checked_int(-1.0, "x").unwrap(), -1);
    }
}
