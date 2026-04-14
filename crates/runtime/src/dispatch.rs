//! Free functions for V8 dispatch and resolution.
//!
//! These are free functions (NOT methods) that take `(scope, state, ...)` as
//! separate parameters. This avoids borrow conflicts: when `enter_v8` creates a
//! `HandleScope` borrowing `&mut isolate`, no methods on `&self` can be called.
//! By making them free functions with disjoint `(scope, state)` params, Rust
//! can verify the borrows don't overlap.

use crate::state::{DispatchResult, SharedState};

// ---------------------------------------------------------------------------
// dispatch_request
// ---------------------------------------------------------------------------

/// Call the JS dispatch function with `body` and classify the result.
///
/// Returns:
/// - `DispatchResult::Sync(json)` — synchronous result or already-fulfilled promise.
/// - `DispatchResult::Async(promise)` — promise is still pending.
/// - `DispatchResult::Error(msg)` — JS exception, rejected promise, or body too large.
pub fn dispatch_request(
    scope: &mut v8::PinScope,
    _state: &SharedState,
    dispatch_fn: &v8::Global<v8::Function>,
    body: &str,
) -> DispatchResult {
    let arg = match v8::String::new(scope, body) {
        Some(s) => s,
        None => return DispatchResult::Error("Request body too large for V8 string".to_string()),
    };

    let func = v8::Local::new(scope, dispatch_fn);
    let undefined = v8::undefined(scope).into();
    let result = func.call(scope, undefined, &[arg.into()]);

    scope.perform_microtask_checkpoint();

    match result {
        None => DispatchResult::Error("JS dispatch call threw an exception".to_string()),
        Some(val) if val.is_promise() => {
            let promise = v8::Local::<v8::Promise>::try_from(val).unwrap();
            match promise.state() {
                v8::PromiseState::Fulfilled => {
                    let json = promise
                        .result(scope)
                        .to_string(scope)
                        .map(|s| s.to_rust_string_lossy(scope))
                        .unwrap_or_else(|| "[object]".to_string());
                    DispatchResult::Sync(json)
                }
                v8::PromiseState::Rejected => {
                    let msg = promise
                        .result(scope)
                        .to_string(scope)
                        .map(|s| s.to_rust_string_lossy(scope))
                        .unwrap_or_else(|| "Promise rejected".to_string());
                    DispatchResult::Error(msg)
                }
                v8::PromiseState::Pending => {
                    let global_promise = v8::Global::new(scope, promise);
                    DispatchResult::Async(global_promise)
                }
            }
        }
        Some(val) => {
            let json = val
                .to_string(scope)
                .map(|s| s.to_rust_string_lossy(scope))
                .unwrap_or_else(|| "[object]".to_string());
            DispatchResult::Sync(json)
        }
    }
}

// ---------------------------------------------------------------------------
// resolve_op
// ---------------------------------------------------------------------------

/// Resolve a pending promise resolver by op-id.
///
/// Removes the resolver from `state.pending_resolvers`, creates a V8 string
/// from `value`, and resolves the promise. If the value string is too large
/// for V8, the promise is rejected with an error message. Runs a microtask
/// checkpoint after resolution.
pub fn resolve_op(
    scope: &mut v8::PinScope,
    state: &SharedState,
    op_id: u32,
    value: &str,
) {
    let resolver = state.borrow_mut().pending_resolvers.remove(&op_id);
    if let Some(resolver) = resolver {
        let r = v8::Local::new(scope, &resolver);
        match v8::String::new(scope, value) {
            Some(val) => {
                r.resolve(scope, val.into());
            }
            None => {
                let err_msg = v8::String::new(scope, "Op result too large for V8 string")
                    .map(|s| s.into())
                    .unwrap_or_else(|| v8::undefined(scope).into());
                r.reject(scope, err_msg);
            }
        }
        scope.perform_microtask_checkpoint();
    }
}

/// Reject a pending promise resolver by op-id.
///
/// Removes the resolver from `state.pending_resolvers`, creates a V8 `Error`
/// from `error`, and rejects the promise. Runs a microtask checkpoint after
/// rejection.
pub fn reject_op(
    scope: &mut v8::PinScope,
    state: &SharedState,
    op_id: u32,
    error: &str,
) {
    let resolver = state.borrow_mut().pending_resolvers.remove(&op_id);
    if let Some(resolver) = resolver {
        let r = v8::Local::new(scope, &resolver);
        let msg = v8::String::new(scope, error)
            .unwrap_or_else(|| v8::String::new(scope, "unknown error").unwrap());
        let exception = v8::Exception::error(scope, msg);
        r.reject(scope, exception);
        scope.perform_microtask_checkpoint();
    }
}

// ---------------------------------------------------------------------------
// fire_timer_callback
// ---------------------------------------------------------------------------

/// Fire a timer callback by timer-id.
///
/// Removes the callback from `state.timer_callbacks`. If found, calls the
/// function with `undefined` as `this` and no arguments. For `setInterval`
/// timers (`interval.is_some()`), re-inserts the callback for the next fire.
/// Runs a microtask checkpoint after the call.
pub fn fire_timer_callback(
    scope: &mut v8::PinScope,
    state: &SharedState,
    timer_id: u32,
) {
    let cb_opt = state.borrow_mut().timer_callbacks.remove(&timer_id);
    if let Some(cb) = cb_opt {
        let func = v8::Local::new(scope, &cb.callback);
        let undefined = v8::undefined(scope).into();
        func.call(scope, undefined, &[]);
        scope.perform_microtask_checkpoint();

        // setInterval: re-insert so next fire can retrieve it
        if cb.interval.is_some() {
            state.borrow_mut().timer_callbacks.insert(timer_id, cb);
        }
    }
}

// ---------------------------------------------------------------------------
// extract_promise_result
// ---------------------------------------------------------------------------

/// Extract the result of a settled promise.
///
/// Returns `Ok(json)` if fulfilled, `Err(msg)` if rejected or still pending.
pub fn extract_promise_result(
    scope: &mut v8::PinScope,
    promise: &v8::Global<v8::Promise>,
) -> Result<String, String> {
    let local = v8::Local::new(scope, promise);
    match local.state() {
        v8::PromiseState::Fulfilled => {
            let json = local
                .result(scope)
                .to_string(scope)
                .map(|s| s.to_rust_string_lossy(scope))
                .unwrap_or_else(|| "[object]".to_string());
            Ok(json)
        }
        v8::PromiseState::Rejected => {
            let msg = local
                .result(scope)
                .to_string(scope)
                .map(|s| s.to_rust_string_lossy(scope))
                .unwrap_or_else(|| "Promise rejected".to_string());
            Err(msg)
        }
        v8::PromiseState::Pending => Err("Promise still pending".to_string()),
    }
}
