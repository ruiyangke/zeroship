//! Auth primitives — `zeroship.auth.getUser()` and `zeroship.auth.requireUser()`.
//!
//! The gateway extracts the authenticated user from the `__zs_session` cookie
//! and forwards it as the `X-ZS-User` header (base64-encoded JSON). The worker
//! decodes this header before dispatching to V8 and stores the user JSON in a
//! thread-local. The V8 callbacks read from the thread-local.
//!
//! This is entirely synchronous — no async, no network, no native calls.

use std::cell::RefCell;

// ---------------------------------------------------------------------------
// Thread-local storage for the current request's authenticated user
// ---------------------------------------------------------------------------

thread_local! {
    /// The authenticated user for the current request, as a JSON string.
    /// Set before V8 dispatch, cleared after. `None` means no user (anonymous).
    static AUTH_USER_JSON: RefCell<Option<String>> = const { RefCell::new(None) };
}

/// Set the authenticated user for the current request.
/// Called by the worker dispatch handler after decoding `X-ZS-User`.
pub fn set_auth_user(user_json: Option<String>) {
    AUTH_USER_JSON.with(|u| *u.borrow_mut() = user_json);
}

/// Clear the authenticated user after request dispatch completes.
pub fn clear_auth_user() {
    AUTH_USER_JSON.with(|u| *u.borrow_mut() = None);
}

// ---------------------------------------------------------------------------
// V8 callbacks
// ---------------------------------------------------------------------------

/// `zeroship.auth.getUser()` — returns the authenticated user object or null.
///
/// Reads the thread-local user JSON set by the worker before dispatch.
/// Returns the parsed object to JS, or `null` if no user is present.
pub fn get_user_callback(
    scope: &mut v8::PinScope,
    _args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let json_opt = AUTH_USER_JSON.with(|u| u.borrow().clone());

    match json_opt {
        Some(json) => {
            let json_str = match v8::String::new(scope, &json) {
                Some(s) => s,
                None => {
                    rv.set(v8::null(scope).into());
                    return;
                }
            };
            let parsed = v8::json::parse(scope, json_str);
            match parsed {
                Some(val) => rv.set(val),
                None => rv.set(v8::null(scope).into()),
            }
        }
        None => {
            rv.set(v8::null(scope).into());
        }
    }
}

/// `zeroship.auth.requireUser()` — returns the authenticated user or throws 401.
///
/// Same as `getUser()` but throws an Error if no user is present.
/// The error message includes a status hint so the gateway can redirect.
pub fn require_user_callback(
    scope: &mut v8::PinScope,
    _args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let json_opt = AUTH_USER_JSON.with(|u| u.borrow().clone());

    match json_opt {
        Some(json) => {
            let json_str = match v8::String::new(scope, &json) {
                Some(s) => s,
                None => {
                    let msg = v8::String::new(scope, "Authentication required").unwrap();
                    let exc = v8::Exception::error(scope, msg);
                    scope.throw_exception(exc);
                    return;
                }
            };
            let parsed = v8::json::parse(scope, json_str);
            match parsed {
                Some(val) => rv.set(val),
                None => {
                    let msg = v8::String::new(scope, "Authentication required").unwrap();
                    let exc = v8::Exception::error(scope, msg);
                    scope.throw_exception(exc);
                }
            }
        }
        None => {
            let msg = v8::String::new(scope, "Authentication required").unwrap();
            let exc = v8::Exception::error(scope, msg);
            scope.throw_exception(exc);
        }
    }
}
