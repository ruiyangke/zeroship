//! DOM primitives shared across web APIs.
//!
//! Hosts the foundational classes that the browser's DOM module
//! exposes and that several Web APIs (fetch, WebSocket, EventSource,
//! MessagePort, XHR, …) build on:
//!
//!   - `EventTarget` (DOM §2.7) — base class, provides
//!     addEventListener/removeEventListener/dispatchEvent.
//!   - `Event` (DOM §2.2) — propagation primitive.
//!   - `AbortController` / `AbortSignal` (DOM §3.3) — cooperative
//!     cancellation. AbortSignal inherits EventTarget.
//!
//! Per fetch-native v2 fix MAJOR-41: these live under `dom/`, NOT
//! under `fetch/`, because they are shared across many Web APIs.
//!
//! ## Install order
//!
//! `install_globals` MUST install in this order: EventTarget, Event,
//! AbortSignal (which inherits EventTarget at template install time),
//! AbortController. The order matters because:
//!
//!   - AbortSignal's `#[v8_inherit(EventTarget)]` resolves the
//!     EventTarget template by calling `EventTarget::install`, which
//!     is idempotent per isolate but must happen at least once.
//!     Calling it from setup_globals first guarantees the parent
//!     template is the same object the global `EventTarget` is bound
//!     to — required for `signal instanceof EventTarget` to walk the
//!     same prototype chain.
//!   - Event must exist before AbortSignal mints "abort" events.
//!   - AbortController instantiates AbortSignal in its constructor,
//!     so AbortSignal must be installed first.

pub mod abort_controller;
pub mod abort_signal;
pub mod event;
pub mod event_target;
pub mod form_data;

/// Install the DOM primitives on `globalThis` in the spec-mandated
/// order: EventTarget → Event → AbortSignal → AbortController →
/// FormData. FormData is order-independent (no inheritance, no
/// dependency on the others) but lives here because it's a
/// DOM-adjacent primitive shared by fetch and XHR.
pub fn install_globals<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    global: v8::Local<v8::Object>,
) {
    event_target::install_global(scope, global);
    install_class(scope, global, "Event", event::Event::install);
    abort_signal::install_global(scope, global);
    abort_controller::install_global(scope, global);
    form_data::install_global(scope, global);
}

fn install_class<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    global: v8::Local<v8::Object>,
    name: &str,
    install_fn: fn(&mut v8::PinScope<'s, '_>) -> v8::Local<'s, v8::FunctionTemplate>,
) {
    let tmpl = install_fn(scope);
    let class_fn = tmpl.get_function(scope).unwrap();

    // Install Event constants on the constructor function.
    if name == "Event" {
        event::install_event_constants(scope, class_fn);
    }

    let key = v8::String::new(scope, name).unwrap();
    global.set(scope, key.into(), class_fn.into());
}
