//! A panic inside a V8 callback throws into script instead of aborting the
//! process.
//!
//! V8 calls native callbacks through `extern "C"` trampolines, and a Rust
//! panic cannot unwind across one: the process aborts, and with it every
//! tenant isolate the thread and the process host. `guard` runs a callback
//! body under `catch_unwind` and turns a panic into a thrown `Error`, so the
//! failure stays inside the isolate that caused it.
//!
//! [`function`], [`function_builder`], [`template`] and
//! [`template_builder`] are the only way a callback is registered with V8:
//! each runs the callback under `guard`, and Clippy rejects V8's own
//! constructors for functions and function templates everywhere else in the
//! workspace (`clippy.toml`, `disallowed-methods`), so a registration that
//! skips the boundary does not pass the lint gate.
//!
//! Catching is sound here because the unwind never leaves Rust: the body is
//! the outermost Rust frame of the callback, so the panic stops before the
//! trampoline, and every nested callback that re-enters Rust from script has
//! its own guard. On the way out the unwind drops the body's handle scopes,
//! `TryCatch` scopes and `RefCell` borrows in order, so the isolate and the
//! callback's native state are left unborrowed. What a panic can leave behind
//! is a half-applied update to that state, which is why this is a backstop and
//! not a substitute for returning early where a V8 call can fail, and why the
//! isolate does not keep serving: `guard` marks it ([`CaughtPanicMark`]),
//! and the runtime that owns it fails the requests still pending on it and
//! quarantines it, so its host replaces it before the next request. The
//! script already running when the panic is thrown keeps running until it
//! returns to the runtime, and may catch the error; the runtime checks the
//! mark when that window ends, before it enters the isolate again.
//!
//! # Native code V8 calls outside this boundary
//!
//! The boundary covers function callbacks. V8 also calls native code through
//! entry points that are not function callbacks, and a panic in one of those
//! still aborts the process. Creator code can reach each of them. Two, and why
//! neither panics today:
//!
//! - `workflows_named_getter` in `zeroship-workflow-v8`, the named-property
//!   interceptor behind `env.workflows.<Name>`, runs for every property a
//!   creator reads there. It is safe today because every step that can fail
//!   returns through `Option` or answers `undefined`, and the class templates
//!   it installs unwrap only the allocation of fixed, short names.
//! - The `throw_data_clone_error` delegate in `web::structured_clone`, which
//!   V8's serializer calls when `structuredClone` meets a value it cannot
//!   clone. It is safe today because its fallible steps, allocating the
//!   `DOMException` and reading its class's own `prototype`, fail only with
//!   an exception or a termination already pending, and V8's serializer
//!   returns before calling the delegate when either is.
//!
//! The others are module resolution and the dynamic-import hook, synthetic
//! module evaluation steps, the near-heap-limit callback, `v8::Weak`
//! finalizers, and the Fast API shims emitted for `#[v8_method(fastcall)]`.

use std::cell::Cell;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::atomic::{AtomicU64, Ordering};

/// Panics `guard` has caught, process-wide and monotonic. Each one is a
/// defect script reached in native code. The worker exports it as
/// `zeroship_runtime_callback_panics_total`, and each one also writes an
/// error log line naming the callback.
static CAUGHT_PANICS: AtomicU64 = AtomicU64::new(0);

/// How many callback panics `guard` has turned into thrown errors in this
/// process.
pub fn caught_panics() -> u64 {
    CAUGHT_PANICS.load(Ordering::Relaxed)
}

/// An isolate slot `guard` sets when it catches a panic in that isolate.
///
/// The runtime installs one per isolate and takes it after every window in
/// which the isolate ran script; a set mark stops the isolate. An isolate
/// without the slot (a bare isolate a test drives) is only logged.
#[derive(Default)]
pub(crate) struct CaughtPanicMark(Cell<bool>);

impl CaughtPanicMark {
    /// Whether a callback panicked in `isolate` since the last call,
    /// clearing the mark.
    pub(crate) fn take(isolate: &v8::Isolate) -> bool {
        isolate.get_slot::<Self>().is_some_and(|mark| mark.0.replace(false))
    }
}

/// A function V8 can call back: the shape of every registered callback.
pub trait Callback:
    for<'s, 'i> Fn(
        &mut v8::PinScope<'s, 'i>,
        v8::FunctionCallbackArguments<'s>,
        v8::ReturnValue<'s, v8::Value>,
    ) + Copy
{
}

impl<F> Callback for F where
    F: for<'s, 'i> Fn(
            &mut v8::PinScope<'s, 'i>,
            v8::FunctionCallbackArguments<'s>,
            v8::ReturnValue<'s, v8::Value>,
        ) + Copy
{
}

/// `callback` with its body run under `guard`. The adapter is zero-sized
/// like `callback` itself, as V8's callback mapping requires.
fn guarded<F: Callback>(callback: F) -> impl Callback {
    move |scope: &mut v8::PinScope<'_, '_>, args, rv| {
        guard(scope, std::any::type_name::<F>(), move |scope| callback(scope, args, rv));
    }
}

/// A JS function that runs `callback` under `guard`; V8's
/// `Function::new`.
#[expect(clippy::disallowed_methods, reason = "the panic boundary's own registration")]
pub fn function<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    callback: impl Callback,
) -> Option<v8::Local<'s, v8::Function>> {
    v8::Function::new(scope, guarded(callback))
}

/// A builder for a JS function that runs `callback` under `guard`; V8's
/// `Function::builder`.
#[expect(clippy::disallowed_methods, reason = "the panic boundary's own registration")]
pub fn function_builder<'s>(callback: impl Callback) -> v8::FunctionBuilder<'s, v8::Function> {
    v8::Function::builder(guarded(callback))
}

/// A function template whose functions run `callback` under `guard`;
/// V8's `FunctionTemplate::new`.
#[expect(clippy::disallowed_methods, reason = "the panic boundary's own registration")]
pub fn template<'s>(
    scope: &v8::PinScope<'s, '_, ()>,
    callback: impl Callback,
) -> v8::Local<'s, v8::FunctionTemplate> {
    v8::FunctionTemplate::new(scope, guarded(callback))
}

/// A builder for a function template whose functions run `callback` under
/// `guard`; V8's `FunctionTemplate::builder`.
#[expect(clippy::disallowed_methods, reason = "the panic boundary's own registration")]
pub fn template_builder<'s>(callback: impl Callback) -> v8::FunctionBuilder<'s, v8::FunctionTemplate> {
    v8::FunctionTemplate::builder(guarded(callback))
}

/// Run `body` as the callback named `site`: the panic boundary every
/// registration in this module installs.
///
/// A panic inside it is logged, marks the isolate for quarantine, and becomes
/// a thrown `Error("internal error")`, unless the isolate is already
/// terminating: throwing would replace the termination and let script
/// resume.
fn guard<'s, 'i>(
    scope: &mut v8::PinScope<'s, 'i>,
    site: &'static str,
    body: impl FnOnce(&mut v8::PinScope<'s, 'i>),
) {
    let Err(panic) = catch_unwind(AssertUnwindSafe(|| body(scope))) else {
        return;
    };
    CAUGHT_PANICS.fetch_add(1, Ordering::Relaxed);
    if let Some(mark) = scope.get_slot::<CaughtPanicMark>() {
        mark.0.set(true);
    }
    tracing::error!(
        site,
        panic = %crate::panic_util::panic_message(&*panic),
        "panic in a V8 callback; thrown into the isolate, which is then quarantined",
    );
    if scope.is_execution_terminating() {
        return;
    }
    let message = crate::strings::message(scope, "internal error");
    let exception = v8::Exception::error(scope, message);
    scope.throw_exception(exception);
}
