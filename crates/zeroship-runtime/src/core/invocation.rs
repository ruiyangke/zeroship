//! Invocation-local request identity carried by V8 continuations.
//!
//! V8 drains an isolate-wide microtask queue. The Rust field describing the
//! host turn that initiated a checkpoint therefore cannot identify every JS
//! continuation that runs inside it. This module stores the owning request
//! and authenticated user in ContinuationPreservedEmbedderData so V8 switches
//! them together with each continuation.

use crate::node::async_hooks::als::{clone_map, read_context_map};
use crate::state::SharedState;

/// Platform-owned identity for one user-code invocation.
#[derive(Clone, Debug, Default)]
pub(crate) struct InvocationContext {
    pub request_id: Option<u64>,
    pub user_json: Option<String>,
}

impl InvocationContext {
    pub(crate) fn request(request_id: u64, user_json: Option<String>) -> Self {
        Self {
            request_id: Some(request_id),
            user_json,
        }
    }

    pub(crate) fn connection(user_json: Option<String>) -> Self {
        Self {
            request_id: None,
            user_json,
        }
    }

    pub(crate) fn from_request_id(state: &SharedState, request_id: Option<u64>) -> Self {
        let user_json = request_id.and_then(|id| state.borrow().per_request_user.get(&id).cloned());
        Self {
            request_id,
            user_json,
        }
    }
}

struct InvocationKeySlot {
    key: v8::Global<v8::Symbol>,
}

fn invocation_key<'s>(scope: &mut v8::PinScope<'s, '_>) -> v8::Local<'s, v8::Symbol> {
    if let Some(slot) = scope.get_slot::<InvocationKeySlot>() {
        return v8::Local::new(scope, slot.key.clone());
    }

    let description = v8::String::new(scope, "zs:InvocationContext").unwrap();
    let key = v8::Symbol::new(scope, Some(description));
    let key_global = v8::Global::new(scope, key);
    scope.set_slot(InvocationKeySlot { key: key_global });
    key
}

fn encode_context<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    context: &InvocationContext,
) -> v8::Local<'s, v8::Array> {
    let request_id: v8::Local<v8::Value> = match context.request_id {
        Some(id) => v8::String::new(scope, &id.to_string())
            .map(Into::into)
            .unwrap_or_else(|| v8::null(scope).into()),
        None => v8::null(scope).into(),
    };
    let user_json: v8::Local<v8::Value> = match context.user_json.as_deref() {
        Some(json) => v8::String::new(scope, json)
            .map(Into::into)
            .unwrap_or_else(|| v8::null(scope).into()),
        None => v8::null(scope).into(),
    };
    v8::Array::new_with_elements(scope, &[request_id, user_json])
}

/// Install an invocation context while `body` enters user code.
///
/// The existing Map is cloned so user `AsyncLocalStorage` entries remain
/// visible and sibling continuations retain immutable snapshots.
pub(crate) fn with_context<'s, R>(
    scope: &mut v8::PinScope<'s, '_>,
    context: &InvocationContext,
    body: impl FnOnce(&mut v8::PinScope<'s, '_>) -> R,
) -> R {
    let previous = enter_context(scope, context);

    let result = body(scope);

    let previous = v8::Local::new(scope, &previous);
    scope.set_continuation_preserved_embedder_data(previous);
    result
}

/// Install an invocation frame and return the exact prior CPED value.
fn enter_context(
    scope: &mut v8::PinScope,
    context: &InvocationContext,
) -> v8::Global<v8::Value> {
    let previous = scope.get_continuation_preserved_embedder_data();
    let previous = v8::Global::new(scope, previous);

    let next = match read_context_map(scope) {
        Some(map) => clone_map(scope, map),
        None => v8::Map::new(scope),
    };
    let key = invocation_key(scope);
    let value = encode_context(scope, context);
    next.set(scope, key.into(), value.into());
    scope.set_continuation_preserved_embedder_data(next.into());
    previous
}

/// Run module initialization under an authoritative invocation context while
/// retaining ambient ALS updates made by module code.
///
/// The post-call Map is cloned before the invocation key is removed. Async
/// work created by `body` therefore keeps its captured invocation frame, while
/// later dispatches inherit module-level `AsyncLocalStorage.enterWith()` data.
pub(crate) fn with_context_preserving_ambient<'s, R>(
    scope: &mut v8::PinScope<'s, '_>,
    context: &InvocationContext,
    body: impl FnOnce(&mut v8::PinScope<'s, '_>) -> R,
) -> R {
    let previous = enter_context(scope, context);
    let result = body(scope);

    if let Some(current) = read_context_map(scope) {
        let ambient = clone_map(scope, current);
        let key = invocation_key(scope);
        ambient.delete(scope, key.into());
        scope.set_continuation_preserved_embedder_data(ambient.into());
    } else {
        let previous = v8::Local::new(scope, &previous);
        scope.set_continuation_preserved_embedder_data(previous);
    }

    result
}

/// Snapshot the complete continuation context for a host callback registered
/// by the current invocation.
pub(crate) fn capture_context(scope: &mut v8::PinScope) -> v8::Global<v8::Value> {
    let current = scope.get_continuation_preserved_embedder_data();
    v8::Global::new(scope, current)
}

/// Enter a previously captured continuation context for a host callback.
pub(crate) fn with_captured_context<'s, R>(
    scope: &mut v8::PinScope<'s, '_>,
    captured: &v8::Global<v8::Value>,
    body: impl FnOnce(&mut v8::PinScope<'s, '_>) -> R,
) -> R {
    let previous = scope.get_continuation_preserved_embedder_data();
    let previous = v8::Global::new(scope, previous);
    let captured = v8::Local::new(scope, captured);
    scope.set_continuation_preserved_embedder_data(captured);

    let result = body(scope);

    let previous = v8::Local::new(scope, &previous);
    scope.set_continuation_preserved_embedder_data(previous);
    result
}

/// Read the context V8 selected for the currently executing continuation.
///
/// Key presence is authoritative. A malformed platform value becomes an
/// anonymous context rather than falling through to isolate-global state.
pub(crate) fn current_context(scope: &mut v8::PinScope) -> Option<InvocationContext> {
    let map = read_context_map(scope)?;
    let key = invocation_key(scope);
    if !map.has(scope, key.into()).unwrap_or(false) {
        return None;
    }

    let malformed = || Some(InvocationContext::default());
    let Some(value) = map.get(scope, key.into()) else {
        return malformed();
    };
    let Ok(array) = v8::Local::<v8::Array>::try_from(value) else {
        return malformed();
    };
    if array.length() < 2 {
        return malformed();
    }

    let Some(request_id_value) = array.get_index(scope, 0) else {
        return malformed();
    };
    let request_id = if request_id_value.is_null() {
        None
    } else if request_id_value.is_string() {
        match request_id_value.to_rust_string_lossy(scope).parse::<u64>() {
            Ok(id) => Some(id),
            Err(_) => return malformed(),
        }
    } else {
        return malformed();
    };

    let Some(user_value) = array.get_index(scope, 1) else {
        return malformed();
    };
    let user_json = if user_value.is_null() {
        None
    } else if user_value.is_string() {
        Some(user_value.to_rust_string_lossy(scope))
    } else {
        return malformed();
    };

    Some(InvocationContext {
        request_id,
        user_json,
    })
}

/// Resolve the owning request for a native callback. Invocation-local state
/// wins; the Rust field remains as a fallback for host turns that predate a
/// captured frame.
pub(crate) fn current_request_id(
    scope: &mut v8::PinScope,
    state: &SharedState,
) -> Option<u64> {
    match current_context(scope) {
        Some(context) => context.request_id,
        None => state.borrow().executing_request_id,
    }
}
