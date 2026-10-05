//! Host-owned RPC signals and per-runtime eviction cancellation.
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
use std::rc::Rc;
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

/// The in-flight RPC signals of one runtime, keyed by that runtime's own
/// request id.
///
/// A runtime owns its registry. The storage is shared with the `AbortGuard`s
/// the registry hands out, so a guard removes only the entry its own runtime
/// registered. Two runtimes on one thread keep separate maps even when they
/// serve the same app id and mint the same request ids.
#[derive(Clone, Default)]
pub(crate) struct AbortRegistry {
    entries: Rc<RefCell<HashMap<u64, RequestSignal>>>,
}

impl AbortRegistry {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Register the native signal returned by `mint_rpc_ctx` for the lifetime
    /// of a request, including its response stream.
    pub(crate) fn register(&self, request_id: u64, signal: RequestSignal) -> AbortGuard {
        self.entries.borrow_mut().insert(request_id, signal);
        AbortGuard {
            request_id,
            entries: Rc::clone(&self.entries),
        }
    }

    /// Abort this runtime's in-flight requests before its isolate is disposed.
    /// Listener exceptions are contained so eviction can finish.
    pub(crate) fn entered_for_eviction(&self, scope: &mut v8::PinScope, app_id: Option<&AppId>) {
        let to_abort: Vec<_> = self.entries.borrow_mut().drain().collect();

        for (request_id, signal) in to_abort {
            v8::tc_scope!(let tc, scope);
            let reason = crate::dom::abort_signal::build_abort_error(tc);
            signal.abort(tc, reason.into());
            if tc.has_caught() {
                tracing::warn!(
                    app_id = app_id.map(AppId::as_str).unwrap_or_default(),
                    request_id,
                    "rpc abort: listener threw during eviction"
                );
            }
        }
    }

    /// Number of in-flight signals currently registered on this runtime.
    #[must_use]
    pub(crate) fn len(&self) -> usize {
        self.entries.borrow().len()
    }
}

/// Keeps the request signal registered for eviction. Dropping the guard
/// unregisters it from its own runtime's registry; dropping a guard after
/// eviction has no effect.
#[must_use = "retain the guard while the request is in flight"]
pub(crate) struct AbortGuard {
    request_id: u64,
    entries: Rc<RefCell<HashMap<u64, RequestSignal>>>,
}

impl Drop for AbortGuard {
    fn drop(&mut self) {
        self.entries.borrow_mut().remove(&self.request_id);
    }
}
