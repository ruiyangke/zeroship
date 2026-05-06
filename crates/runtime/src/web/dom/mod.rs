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
//! These live under `dom/`, not
//! under `fetch/`, because they are shared across many Web APIs.
//!
//! ## Install order
//!
//! `install_globals` MUST install in this order: EventTarget, Event,
//! CustomEvent, AbortSignal (which inherits EventTarget at template
//! install time), AbortController. The order matters because:
//!
//!   - AbortSignal's `#[v8_inherit(EventTarget)]` resolves the
//!     EventTarget template by calling `EventTarget::install`, which
//!     is idempotent per isolate but must happen at least once.
//!     Calling it from setup_globals first guarantees the parent
//!     template is the same object the global `EventTarget` is bound
//!     to — required for `signal instanceof EventTarget` to walk the
//!     same prototype chain.
//!   - Event must exist before CustomEvent (which inherits via
//!     `#[v8_inherit(Event)]`) AND before AbortSignal mints "abort"
//!     events.
//!   - CustomEvent's template must be installed AFTER Event for the
//!     same `__InstallSlot_Event`-cache reason as AbortSignal:
//!     `e instanceof Event` walks the same prototype chain only when
//!     both the global `Event` binding and CustomEvent's `inherit`
//!     call resolve the SAME FunctionTemplate.
//!   - AbortController instantiates AbortSignal in its constructor,
//!     so AbortSignal must be installed first.

pub mod abort_controller;
pub mod abort_signal;
pub mod close_event;
pub mod custom_event;
pub mod event;
pub mod event_target;
pub mod exception;
pub mod form_data;
pub mod message_event;

/// Install the DOM primitives on `globalThis` in the spec-mandated
/// order: DOMException → EventTarget → Event → CustomEvent →
/// AbortSignal → AbortController → FormData. DOMException is
/// installed first because AbortSignal's mint helpers construct
/// DOMException-shaped abort reasons via the global constructor — if
/// the class isn't there yet, those mints fall back to a plain Error.
/// FormData is order-independent (no inheritance, no dependency on
/// the others) but lives here because it's a DOM-adjacent primitive
/// shared by fetch and XHR.
pub fn install_globals<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    global: v8::Local<v8::Object>,
) {
    // #198 — simple classes (template + globalThis bind only) collapse
    // onto the macro-emitted `register` fn. Classes that need extras
    // stay on their `install_global` / `install_class` callsites:
    //   - DOMException — Error-proto chain + legacy code constants;
    //   - Event        — Event interface constants (NONE, AT_TARGET, …);
    //   - MessageEvent /
    //     CloseEvent   — state-marker name mismatch (global bound under
    //                    the IDL name, `class_ty` is e.g. `MessageEventState`);
    //   - AbortSignal  — `onabort` IDL accessor pair (defineProperty);
    //   - FormData     — 4 hand-rolled scope-aware methods.
    //
    // Install order: EventTarget before AbortSignal (which inherits via
    // `#[v8_inherit(EventTarget)]`); Event before CustomEvent /
    // MessageEvent / CloseEvent (same reason).
    exception::install_global(scope, global);
    crate::register_native_classes!(scope, global, [
        event_target::EventTarget,
    ]);
    install_class(scope, global, "Event", event::Event::install);
    crate::register_native_classes!(scope, global, [
        custom_event::CustomEvent,
    ]);
    // MessageEvent / CloseEvent inherit Event via #[v8_inherit(Event)] —
    // installed AFTER Event for the same `__InstallSlot_Event`-cache
    // reason as CustomEvent.
    install_class(
        scope,
        global,
        "MessageEvent",
        message_event::MessageEventState::install,
    );
    install_class(
        scope,
        global,
        "CloseEvent",
        close_event::CloseEventState::install,
    );
    abort_signal::install_global(scope, global);
    crate::register_native_classes!(scope, global, [
        abort_controller::AbortController,
    ]);
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
