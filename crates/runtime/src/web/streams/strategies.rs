//! `ByteLengthQueuingStrategy` and `CountQueuingStrategy` — spec §6.2 / §6.3.
//!
//! Two near-identical classes:
//! - both expose `highWaterMark: number` and `size: Function`
//! - both take `{ highWaterMark }` in the constructor (required dictionary
//!   member per WebIDL §3.2.20 — missing throws TypeError per critic #33)
//! - the `size` Function is **shared per realm** (WPT
//!   `queuing-strategies-size-function-per-global.window.js`):
//!   `Object.is(a.size, b.size) === true` for two instances within one realm.
//!
//! ## Per-realm shared function
//!
//! WPT requires `Object.is(s1.size, s2.size) === true` for same-realm
//! strategies. We implement this by stashing the canonical Function on
//! `globalThis` under a per-class private symbol (so it's reachable from
//! every getter call, but invisible to JS enumeration). The function is
//! built once at install time by `install_*_queuing_strategy`.
//!
//! ## ByteLength's `size`
//!
//! Per spec §6.2.5: `size(chunk)` returns `chunk.byteLength`. This is a
//! direct property read (no validation, no coercion) — for a plain object
//! with no `byteLength`, the result is `undefined`. Our implementation
//! matches this verbatim: read property, return whatever it is. The
//! "throws on non-buffer" path the critic referenced applies inside
//! `EnqueueValueWithSize`'s downstream `IsNonNegativeNumber` check, NOT
//! inside `size` itself.
//!
//! ## Count's `size`
//!
//! Per spec §6.3.5: `size()` returns `1`. Always. No reading of `chunk`,
//! no coercion.

use zeroship_runtime_macros::{v8_class, WebIdlDict};
#[allow(unused_imports)]
use zeroship_runtime_macros::{v8_constructor, v8_getter};

use crate::state::OpError;

// Slot name on `globalThis` where the canonical Function is cached. Using
// the class name plus `__sizeFn` keeps the two classes' caches distinct
// and avoids collision with any user-installed property.
const BYTE_LENGTH_SIZE_SLOT: &str = "[[ByteLengthQueuingStrategy.sizeFn]]";
const COUNT_SIZE_SLOT: &str = "[[CountQueuingStrategy.sizeFn]]";

// ---------------------------------------------------------------------------
// ByteLengthQueuingStrategy
// ---------------------------------------------------------------------------

#[allow(missing_debug_implementations)]
pub struct ByteLengthQueuingStrategy {
    high_water_mark: f64,
}

#[v8_class]
impl ByteLengthQueuingStrategy {
    /// `new ByteLengthQueuingStrategy(init: { highWaterMark: number })`
    ///
    /// Per WebIDL §3.2.20 + critic #33:
    /// - `init` is a required argument; missing → TypeError.
    /// - `init.highWaterMark` is a required dictionary member; missing
    ///   → TypeError.
    /// - The value is `unrestricted double` (so NaN, ±∞, negative values
    ///   are all allowed at this layer; enforcement happens in the
    ///   controller's enqueue path).
    #[v8_constructor]
    fn new(
        scope: &mut v8::PinScope,
        init: v8::Local<v8::Value>,
    ) -> Result<Self, OpError> {
        let hwm = parse_queuing_strategy_init(scope, init, "ByteLengthQueuingStrategy")?;
        Ok(ByteLengthQueuingStrategy {
            high_water_mark: hwm,
        })
    }

    /// `highWaterMark` — return the user-supplied number unchanged.
    #[v8_getter]
    #[allow(non_snake_case)]
    fn highWaterMark(&self) -> f64 {
        self.high_water_mark
    }

    /// `size` — return the per-realm shared Function.
    ///
    /// First call: build the Function, stash on `globalThis` under the
    /// private symbol. Subsequent calls: read from the slot.
    ///
    /// WPT `queuing-strategies-size-function-per-global.window.js`
    /// requires `Object.is(a.size, b.size) === true` for same-realm
    /// strategies. The slot-on-global approach guarantees this without
    /// needing access to the runtime's SharedState (which doesn't exist
    /// in raw-isolate test harnesses).
    #[v8_getter]
    fn size<'s>(&self, scope: &mut v8::PinScope<'s, '_>) -> v8::Local<'s, v8::Value> {
        get_or_create_shared_size_fn(
            scope,
            BYTE_LENGTH_SIZE_SLOT,
            byte_length_size_callback,
        )
    }
}

// ---------------------------------------------------------------------------
// CountQueuingStrategy
// ---------------------------------------------------------------------------

#[allow(missing_debug_implementations)]
pub struct CountQueuingStrategy {
    high_water_mark: f64,
}

#[v8_class]
impl CountQueuingStrategy {
    #[v8_constructor]
    fn new(
        scope: &mut v8::PinScope,
        init: v8::Local<v8::Value>,
    ) -> Result<Self, OpError> {
        let hwm = parse_queuing_strategy_init(scope, init, "CountQueuingStrategy")?;
        Ok(CountQueuingStrategy {
            high_water_mark: hwm,
        })
    }

    #[v8_getter]
    #[allow(non_snake_case)]
    fn highWaterMark(&self) -> f64 {
        self.high_water_mark
    }

    #[v8_getter]
    fn size<'s>(&self, scope: &mut v8::PinScope<'s, '_>) -> v8::Local<'s, v8::Value> {
        get_or_create_shared_size_fn(scope, COUNT_SIZE_SLOT, count_size_callback)
    }
}

// ---------------------------------------------------------------------------
// `size` Function callbacks
// ---------------------------------------------------------------------------

/// `ByteLengthQueuingStrategy.size(chunk)` — spec §6.2.5.
///
/// Spec implementation: `return chunk.byteLength;`. For a plain object
/// with no `byteLength`, this returns `undefined`. Per critic #10 the
/// caller (controller's enqueue) handles the downstream
/// `IsNonNegativeNumber` rejection.
fn byte_length_size_callback(
    scope: &mut v8::PinScope,
    args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    let chunk = args.get(0);
    // Spec literally calls `Get(chunk, "byteLength")`. We perform that
    // unconditionally: any object (including ArrayBufferView,
    // ArrayBuffer, plain {}) with no byteLength returns undefined; an
    // accessor that throws will leave a pending exception which V8
    // surfaces to the caller.
    let key = v8::String::new(scope, "byteLength").unwrap();
    if let Ok(obj) = v8::Local::<v8::Object>::try_from(chunk) {
        match obj.get(scope, key.into()) {
            Some(v) => rv.set(v),
            None => {
                // Get threw — V8 has the pending exception set; do
                // nothing, the exception propagates.
            }
        }
    } else {
        // Non-object chunk (number, string, undefined, etc.). Spec
        // `Get(chunk, "byteLength")` on a primitive boxes it; `String`
        // has `length` but not `byteLength`, so the result is undefined.
        // We return undefined directly (V8's default for unset rv).
    }
}

/// `CountQueuingStrategy.size()` — spec §6.3.5: returns 1.
fn count_size_callback(
    scope: &mut v8::PinScope,
    _args: v8::FunctionCallbackArguments,
    mut rv: v8::ReturnValue,
) {
    rv.set(v8::Integer::new(scope, 1).into());
}

// ---------------------------------------------------------------------------
// Per-realm size Function caching
// ---------------------------------------------------------------------------

/// Get the shared `size` Function for this realm, building and caching it
/// on first call.
///
/// Cache key: a private symbol named `slot_name` on `globalThis`. The
/// private symbol is invisible to JS enumeration (via `Object.keys` or
/// any property-access loop), so this doesn't observably mutate the
/// global namespace.
fn get_or_create_shared_size_fn<'s>(
    scope: &mut v8::PinScope<'s, '_>,
    slot_name: &'static str,
    callback: impl v8::MapFnTo<v8::FunctionCallback>,
) -> v8::Local<'s, v8::Value> {
    let global = scope.get_current_context().global(scope);
    let priv_ = crate::streams::slots::private_sym(scope, slot_name);

    if let Some(cached) = global.get_private(scope, priv_) {
        if !cached.is_undefined() {
            return cached;
        }
    }

    // First call in this realm: build the Function and cache.
    let tmpl = v8::FunctionTemplate::new(scope, callback);
    let func = tmpl.get_function(scope).unwrap();
    global.set_private(scope, priv_, func.into());
    func.into()
}

// ---------------------------------------------------------------------------
// QueuingStrategyInit dictionary parsing
// ---------------------------------------------------------------------------

/// `QueuingStrategyInit` per WebIDL §3.2.20 + critic #33.
///
/// `highWaterMark` is a REQUIRED member per IDL — the macro doesn't
/// emit a "required-throws-if-missing" check today (a non-Option
/// field defaults to `f64::default() == 0.0` on missing key), so we
/// model it as `Option<f64>` and check None at the call site to
/// preserve the spec-mandated TypeError.
#[derive(Default, Debug, WebIdlDict)]
struct QueuingStrategyInit {
    #[webidl_name = "highWaterMark"]
    high_water_mark: Option<f64>,
}

/// Parse `{ highWaterMark: number }` per WebIDL §3.2.20 + critic #33.
///
/// - `init` undefined / missing → TypeError ("init is required")
/// - `init` not an object → TypeError (via the auto-derived `from_v8`)
/// - `init.highWaterMark` undefined → TypeError ("required member missing")
/// - `init.highWaterMark` not a number → ToNumber (NaN / -Infinity / ∞ allowed
///   at this layer; the spec's `unrestricted double` permits them)
fn parse_queuing_strategy_init(
    scope: &mut v8::PinScope,
    init: v8::Local<v8::Value>,
    class_name: &str,
) -> Result<f64, OpError> {
    // Spec mandates that `init` itself is required-throws — the dict
    // converter would happily default-construct from undefined/null,
    // so we keep the explicit pre-check at the call site.
    if init.is_undefined() {
        return Err(OpError::type_error(format!(
            "{class_name}: init argument is required",
        )));
    }
    let parsed = QueuingStrategyInit::from_v8(scope, init).map_err(|e| {
        // Dress the converter's generic message with the class name
        // so test failures point at the offender. Other shapes
        // (TypeError on non-Object) keep the converter's text — the
        // class name is implicit in the surrounding stack trace.
        OpError::type_error(format!("{class_name}: {}", e.message))
    })?;
    let n = parsed.high_water_mark.ok_or_else(|| {
        OpError::type_error(format!(
            "{class_name}: highWaterMark is a required dictionary member",
        ))
    })?;
    Ok(n)
}

// ---------------------------------------------------------------------------
// Public installers — bind the classes to `globalThis`
// ---------------------------------------------------------------------------

/// Install `globalThis.ByteLengthQueuingStrategy`. Called from
/// `setup_globals` (or test harnesses).
pub fn install_byte_length_queuing_strategy(
    scope: &mut v8::PinScope,
    global: v8::Local<v8::Object>,
) {
    let tmpl = ByteLengthQueuingStrategy::install(scope);
    let class_fn = tmpl.get_function(scope).unwrap();
    let key = v8::String::new(scope, "ByteLengthQueuingStrategy").unwrap();
    global.set(scope, key.into(), class_fn.into());
}

/// Install `globalThis.CountQueuingStrategy`.
pub fn install_count_queuing_strategy(
    scope: &mut v8::PinScope,
    global: v8::Local<v8::Object>,
) {
    let tmpl = CountQueuingStrategy::install(scope);
    let class_fn = tmpl.get_function(scope).unwrap();
    let key = v8::String::new(scope, "CountQueuingStrategy").unwrap();
    global.set(scope, key.into(), class_fn.into());
}
