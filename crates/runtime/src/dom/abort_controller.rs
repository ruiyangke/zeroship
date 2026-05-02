//! Native `AbortController` per DOM §3.3
//! (https://dom.spec.whatwg.org/#interface-AbortController).
//!
//! Replaces the JS polyfill that lived in `embed/fetch.js:528-538`.
//! AbortController is a thin lifetime owner over an AbortSignal:
//!
//!   ```js
//!   const c = new AbortController();
//!   c.signal;       // -> AbortSignal (always the same object)
//!   c.abort(reason);// -> runs the signal-abort algorithm on c.signal
//!   ```
//!
//! Storage rule: the signal lives on a private symbol on the
//! controller wrapper (so `c.signal === c.signal` per [SameObject]).
//! Per design §XIII.3 / spec §3.3.

use zeroship_runtime_macros::v8_class;
#[allow(unused_imports)]
use zeroship_runtime_macros::{v8_constructor, v8_getter, v8_method};

// ---------------------------------------------------------------------------
// AbortController state
// ---------------------------------------------------------------------------

/// Backing state for AbortController. The owned signal is stored as
/// a `v8::Global<v8::Object>` so the [SameObject] invariant on the
/// `signal` getter is automatic — we hand back the same JS object
/// on every access.
pub struct AbortController {
    pub(crate) signal: v8::Global<v8::Object>,
}

// AbortController has no Default — every instance owns a freshly
// minted signal. The `#[v8_class]` macro accommodates this via a
// user `#[v8_constructor]`.

// ---------------------------------------------------------------------------
// AbortController IDL surface
// ---------------------------------------------------------------------------

#[v8_class]
impl AbortController {
    /// `new AbortController()` — DOM §3.3. Creates a fresh signal.
    #[v8_constructor]
    fn new(scope: &mut v8::PinScope) -> AbortController {
        let (signal_obj, _raw) = super::abort_signal::mint_abort_signal(scope);
        AbortController {
            signal: v8::Global::new(scope, signal_obj),
        }
    }

    /// `controller.signal` getter — DOM §3.3. [SameObject] preserved
    /// via the stored Global.
    #[v8_getter]
    fn signal<'s>(&self, scope: &mut v8::PinScope<'s, '_>) -> v8::Local<'s, v8::Value> {
        v8::Local::new(scope, self.signal.clone()).into()
    }

    /// `controller.abort(reason?)` — DOM §3.3. Runs the signal-abort
    /// algorithm on the owned signal with the given reason (or the
    /// default AbortError if undefined).
    #[v8_method]
    fn abort(&self, scope: &mut v8::PinScope, reason: v8::Local<v8::Value>) {
        let signal_obj = v8::Local::new(scope, self.signal.clone());
        let resolved_reason: v8::Local<v8::Value> = if reason.is_undefined() {
            super::abort_signal::build_abort_error(scope).into()
        } else {
            reason
        };
        super::abort_signal::signal_abort(scope, signal_obj, resolved_reason);
    }
}

// ---------------------------------------------------------------------------
// install_global
// ---------------------------------------------------------------------------

pub fn install_global<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    global: v8::Local<v8::Object>,
) {
    let tmpl = AbortController::install(scope);
    let class_fn = tmpl.get_function(scope).unwrap();
    let key = v8::String::new(scope, "AbortController").unwrap();
    global.set(scope, key.into(), class_fn.into());
}
