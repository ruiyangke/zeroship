//! Host-owned RPC signals and eviction-time cancellation.
//!
//! The request context and eviction registry retain the same native signal.
//! Cancellation invokes the native abort algorithm without resolving a
//! creator-writable constructor or controller method. Registry entries are
//! removed before listeners run, so reentrant code never borrows the map.
//!
//! Pending requests and response streams retain an `AbortGuard` until their
//! work ends. Eviction fires abort synchronously; it does not await cleanup
//! before the worker disposes the isolate.

use std::cell::RefCell;
use std::collections::HashMap;
use zeroship_core::app_id::AppId;

/// A host-created `AbortSignal`. Its private handle ensures native cancellation
/// only receives objects minted by the signal implementation.
#[derive(Clone)]
pub struct RequestSignal(v8::Global<v8::Object>);

impl RequestSignal {
    pub(crate) fn new(scope: &mut v8::PinScope) -> Self {
        let signal = crate::dom::abort_signal::mint_abort_signal(scope).0;
        Self(v8::Global::new(scope, signal))
    }

    pub(crate) fn local<'s>(&self, scope: &v8::PinScope<'s, '_>) -> v8::Local<'s, v8::Object> {
        v8::Local::new(scope, &self.0)
    }

    pub(crate) fn abort(&self, scope: &mut v8::PinScope, reason: v8::Local<v8::Value>) {
        let signal = self.local(scope);
        crate::dom::abort_signal::signal_abort(scope, signal, reason);
    }
}

thread_local! {
    static REGISTRY: RefCell<HashMap<RegistryKey, RequestSignal>> =
        RefCell::new(HashMap::new());
}

#[derive(Hash, Eq, PartialEq, Clone, Debug)]
struct RegistryKey {
    app_id: AppId,
    request_id: u64,
}

/// Keeps the request signal registered for eviction. Dropping the guard
/// unregisters it; dropping a guard after eviction has no effect.
#[must_use = "retain the guard while the request is in flight"]
pub struct AbortGuard {
    key: RegistryKey,
}

impl Drop for AbortGuard {
    fn drop(&mut self) {
        REGISTRY.with(|registry| {
            registry.borrow_mut().remove(&self.key);
        });
    }
}

/// Register the native signal returned by `mint_rpc_ctx` for the lifetime
/// of a request, including its response stream.
pub fn register_in_flight(app_id: &AppId, request_id: u64, signal: RequestSignal) -> AbortGuard {
    let key = RegistryKey {
        app_id: app_id.clone(),
        request_id,
    };
    REGISTRY.with(|registry| {
        registry.borrow_mut().insert(key.clone(), signal);
    });
    AbortGuard { key }
}

/// Abort in-flight requests before the worker removes this app's isolate.
/// Listener exceptions are contained so eviction can finish.
pub fn entered_for_eviction(scope: &mut v8::PinScope, app_id: &AppId) {
    let to_abort: Vec<_> = REGISTRY.with(|registry| {
        registry
            .borrow_mut()
            .extract_if(|key, _| &key.app_id == app_id)
            .collect()
    });

    for (key, signal) in to_abort {
        v8::tc_scope!(let tc, scope);
        let reason = crate::dom::abort_signal::build_abort_error(tc);
        signal.abort(tc, reason.into());
        if tc.has_caught() {
            tracing::warn!(
                app_id = key.app_id.as_str(),
                request_id = key.request_id,
                "rpc abort: listener threw during eviction"
            );
        }
    }
}

/// Inspect registry occupancy for eviction tests.
#[doc(hidden)]
pub fn entries_for_app(app_id: &AppId) -> usize {
    REGISTRY.with(|registry| {
        registry
            .borrow()
            .keys()
            .filter(|key| &key.app_id == app_id)
            .count()
    })
}
