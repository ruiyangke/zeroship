//! The realm's own interface objects, captured per isolate.
//!
//! When a spec algorithm says "a new `ReadableStream`" or "a new `Response`", it
//! means an object made by the realm's original interface. Script owns the
//! global object, though: it can delete `globalThis.Response`, replace it with
//! a function returning anything, or turn the property into a throwing getter.
//! Internal construction that looks the class up on the global therefore
//! builds whatever script chose, and native code that then reads the result as
//! a `Response` reads some other object's memory.
//!
//! So each interface the runtime constructs internally is recorded here when
//! it is installed, before any script runs: its constructor function and the
//! prototype object it had then. Internal construction goes through
//! [`construct`], and internally built wrappers take their prototype from
//! [`prototype`], so neither consults the global object again.
//!
//! The hand-rolled stream classes also keep their `FunctionTemplate` here
//! ([`template`]), so the wrappers the runtime builds and the ones script
//! constructs share one template, one constructor and one prototype per
//! isolate.

use std::collections::HashMap;

use crate::state::OpError;

/// An interface the runtime constructs or builds wrappers of internally.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Intrinsic {
    ReadableStream,
    ReadableStreamDefaultController,
    ReadableByteStreamController,
    ReadableStreamBYOBRequest,
    WritableStream,
    WritableStreamDefaultController,
    TransformStream,
    TransformStreamDefaultController,
    Request,
    Response,
    Headers,
    Blob,
    File,
    FormData,
    AbortSignal,
    DataView,
    Float16Array,
}

impl Intrinsic {
    /// The interface's name on the global object.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::ReadableStream => "ReadableStream",
            Self::ReadableStreamDefaultController => "ReadableStreamDefaultController",
            Self::ReadableByteStreamController => "ReadableByteStreamController",
            Self::ReadableStreamBYOBRequest => "ReadableStreamBYOBRequest",
            Self::WritableStream => "WritableStream",
            Self::WritableStreamDefaultController => "WritableStreamDefaultController",
            Self::TransformStream => "TransformStream",
            Self::TransformStreamDefaultController => "TransformStreamDefaultController",
            Self::Request => "Request",
            Self::Response => "Response",
            Self::Headers => "Headers",
            Self::Blob => "Blob",
            Self::File => "File",
            Self::FormData => "FormData",
            Self::AbortSignal => "AbortSignal",
            Self::DataView => "DataView",
            Self::Float16Array => "Float16Array",
        }
    }
}

/// One captured interface.
struct Captured {
    constructor: v8::Eternal<v8::Function>,
    prototype: v8::Eternal<v8::Object>,
}

/// The isolate's captured interfaces and cached stream-class templates.
#[derive(Default)]
struct IntrinsicsSlot {
    captured: HashMap<Intrinsic, Captured>,
    templates: HashMap<Intrinsic, v8::Eternal<v8::FunctionTemplate>>,
}

fn slot<'a>(scope: &'a mut v8::PinScope) -> &'a mut IntrinsicsSlot {
    if scope.get_slot::<IntrinsicsSlot>().is_none() {
        scope.set_slot(IntrinsicsSlot::default());
    }
    scope
        .get_slot_mut::<IntrinsicsSlot>()
        .expect("the intrinsics slot was installed above")
}

/// Record `constructor` as the realm's original `which`.
///
/// The object its `prototype` property holds now is recorded with it. Call
/// it where the interface is installed on the global, before script can
/// touch either. A second capture of the same interface keeps the first.
pub fn capture(scope: &mut v8::PinScope, which: Intrinsic, constructor: v8::Local<v8::Function>) {
    if slot(scope).captured.contains_key(&which) {
        return;
    }
    let Some(key) = v8::String::new(scope, "prototype") else {
        return;
    };
    let Some(prototype) = constructor
        .get(scope, key.into())
        .and_then(|value| v8::Local::<v8::Object>::try_from(value).ok())
    else {
        return;
    };
    let constructor_eternal = v8::Eternal::empty();
    constructor_eternal.set(scope, constructor);
    let prototype_eternal = v8::Eternal::empty();
    prototype_eternal.set(scope, prototype);
    slot(scope).captured.insert(
        which,
        Captured { constructor: constructor_eternal, prototype: prototype_eternal },
    );
}

/// The realm's original constructor for `which`, if it was installed.
pub fn constructor<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    which: Intrinsic,
) -> Option<v8::Local<'s, v8::Function>> {
    let captured = scope.get_slot::<IntrinsicsSlot>()?.captured.get(&which)?;
    captured.constructor.get(scope)
}

/// The prototype `which` had when it was installed, if it was.
pub fn prototype<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    which: Intrinsic,
) -> Option<v8::Local<'s, v8::Object>> {
    let captured = scope.get_slot::<IntrinsicsSlot>()?.captured.get(&which)?;
    captured.prototype.get(scope)
}

/// The isolate's template for the hand-rolled class `which`.
///
/// `build` makes it on first use. Building it also captures the class's
/// constructor and prototype, so wrappers built before the class is
/// installed on the global still get the class's own prototype.
pub fn template<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    which: Intrinsic,
    build: fn(&mut v8::PinScope<'s, '_>) -> v8::Local<'s, v8::FunctionTemplate>,
) -> v8::Local<'s, v8::FunctionTemplate> {
    if let Some(cached) = scope
        .get_slot::<IntrinsicsSlot>()
        .and_then(|slot| slot.templates.get(&which))
        .and_then(|eternal| eternal.get(scope))
    {
        return cached;
    }
    let template = build(scope);
    let eternal = v8::Eternal::empty();
    eternal.set(scope, template);
    slot(scope).templates.insert(which, eternal);
    if let Some(constructor) = template.get_function(scope) {
        capture(scope, which, constructor);
    }
    template
}

/// `new which(...args)` through the realm's original constructor.
///
/// The instance is given the interface's captured prototype.
///
/// # Errors
///
/// A `TypeError` when `which` was never installed in this realm, and
/// otherwise what the constructor threw, verbatim (a budget refusal stays the
/// constructor's `RangeError`, for the caller to rethrow or reject with).
pub fn construct(
    scope: &mut v8::PinScope,
    which: Intrinsic,
    args: &[v8::Local<v8::Value>],
) -> Result<v8::Global<v8::Object>, OpError> {
    let name = which.name();
    let class = constructor(scope, which)
        .ok_or_else(|| OpError::type_error(format!("{name} is not installed in this realm")))?;
    let prototype = prototype(scope, which);
    v8::tc_scope!(let tc, scope);
    let Some(instance) = class.new_instance(tc, args) else {
        return Err(tc.exception().map_or_else(
            || OpError::error(format!("new {name} did not complete")),
            |exception| OpError::js_value(tc, exception, format!("new {name} threw")),
        ));
    };
    if let Some(prototype) = prototype {
        instance.set_prototype(tc, prototype.into());
    }
    Ok(v8::Global::new(tc, instance))
}
