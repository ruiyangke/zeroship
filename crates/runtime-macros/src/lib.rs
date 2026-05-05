//! Proc macros for the zeroship runtime.
//!
//! [`v8_class`] wraps an `impl` block as a V8 ObjectTemplate-backed
//! class. Methods, getters, setters, constructors, and static methods
//! get auto-generated callbacks; instance state lives in V8 internal
//! fields.
//!
//! ```ignore
//! struct Headers { /* ... */ }
//!
//! #[v8_class]
//! impl Headers {
//!     #[v8_constructor]
//!     fn new() -> Result<Self, OpError> { ... }
//!
//!     #[v8_method]
//!     fn get(&self, name: String) -> Option<String> { ... }
//!
//!     #[v8_method]
//!     fn set(&mut self, name: String, value: String) -> Result<(), OpError> { ... }
//!
//!     // Async methods compile to a Promise-returning sync V8 callback
//!     // that spawns the body via state.spawned_ops. &mut self is
//!     // rejected at compile time — use &self + Cell/RefCell for state
//!     // that needs to mutate inside the body.
//!     #[v8_async_method]
//!     async fn fetch_remote(&self, url: String) -> Result<Vec<u8>, OpError> { ... }
//! }
//! ```
//!
//! The class macro generates `Headers::install(scope) -> v8::Local<v8::FunctionTemplate>`
//! that the runtime calls during `setup_globals` to wire the class onto
//! `globalThis`. Free-function V8 callbacks are written by hand in the
//! runtime crate (see `crates/runtime/src/core/init.rs` for the
//! patterns: extract `SharedState` via `scope.get_slot`, read JS args
//! with `args.get(i)`, set the return via `rv.set(...)`).

use proc_macro::TokenStream;

mod codegen;
mod known_type;
mod types;
mod v8_class;
mod v8_iterable;
mod webidl_dict;
mod webidl_enum;

// Wave 7 (F9) — insta snapshot tests for the WebIdl derives + v8_iterable.
// Live as siblings of the production module so insta's default snapshot
// resolution lands the .snap files in src/snapshots/. Mirrors
// v8_class/snapshot_tests.rs which covers the v8_class derive.
#[cfg(test)]
mod v8_iterable_tests;
#[cfg(test)]
mod webidl_dict_tests;
#[cfg(test)]
mod webidl_enum_tests;

// Re-exports for the v8_class emit submodules and WebIDL derives.
// Codegen helpers (return-value marshalling, extract codegen, OpError
// throw, must_str variants) all live in `codegen.rs`. Type-classifier
// predicates (is_X / type_ident / first_generic_arg) live in `types.rs`.
pub(crate) use codegen::{
    gen_call_return, gen_extract, gen_extract_throw, gen_throw_op_error_arms, must_str,
    must_str_abs,
};
pub(crate) use types::{
    first_generic_arg, is_byte_string, is_enforce_range_u32, is_enforce_range_u64,
    is_option_usv_string, is_unit_type, is_usv_string, is_vec_u8, is_vec_vec_u8, type_ident, Param,
};

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

/// Wrap an `impl` block as a V8 ObjectTemplate-backed class.
///
/// See module docs for usage. Methods marked with `#[v8_method]`,
/// `#[v8_getter]`, `#[v8_setter]`, and `#[v8_constructor]` get
/// auto-generated callbacks; the macro emits `Self::install(scope) ->
/// v8::Local<v8::FunctionTemplate>` for the runtime to register on the
/// global object.
#[proc_macro_attribute]
pub fn v8_class(attr: TokenStream, item: TokenStream) -> TokenStream {
    v8_class::expand(attr, item)
}

#[proc_macro_attribute]
pub fn v8_method(_attr: TokenStream, item: TokenStream) -> TokenStream {
    // Marker attribute consumed by `#[v8_class]`. When applied to a method
    // outside a `#[v8_class]` impl block this is a no-op (the method stays
    // as written) — the macro doesn't error so editor tooling that
    // pre-expands attribute macros doesn't surface a false positive.
    item
}

/// Marker attribute consumed by `#[v8_class]`: declare an async method
/// whose return value materialises as a Promise on the JS surface.
///
/// The macro emits a sync V8 callback that:
///   1. Allocates a `v8::PromiseResolver`
///   2. Spawns the user's `async fn` body via `state.spawned_ops`
///   3. Returns the Promise immediately
///
/// When the future settles, the runtime pump dequeues an
/// `OpResult::JsValue` and resolves (or rejects) the bound promise. The
/// user's `async fn` body can `.await` freely.
///
/// # Rejected at compile time
///
/// `&mut self` async methods are rejected — borrow across `.await` is
/// unsound under V8 re-entry. The macro emits a `compile_error!` with
/// the suggested fix (use `&self` + `Cell` / `RefCell`). See
/// `crates/runtime/tests/v8_async_method_smoke.rs` for the positive
/// shapes and the runtime-level doctests for the rejection rules.
///
/// Non-`async` methods marked with the attribute are also rejected for
/// the same reason — the call-site emits `.await`, which doesn't
/// type-check on a non-Future return.
///
/// # Allowed return shapes
///
/// `()`, `T`, or `Result<T, OpError>` where
/// `T ∈ { (), bool, u32, i32, f64, String, Vec<u8>, v8::Global<v8::Value> }`.
///
/// Outside a `#[v8_class]` impl block this attribute is a no-op (the
/// fn stays as written) so editor tooling that pre-expands attribute
/// macros doesn't trip a false positive.
#[proc_macro_attribute]
pub fn v8_async_method(_attr: TokenStream, item: TokenStream) -> TokenStream {
    item
}

#[proc_macro_attribute]
pub fn v8_getter(_attr: TokenStream, item: TokenStream) -> TokenStream {
    item
}

#[proc_macro_attribute]
pub fn v8_setter(_attr: TokenStream, item: TokenStream) -> TokenStream {
    item
}

#[proc_macro_attribute]
pub fn v8_constructor(_attr: TokenStream, item: TokenStream) -> TokenStream {
    item
}

/// Marker attribute consumed by `#[v8_class]`: declare a static method
/// installed on the constructor function (not the prototype) per
/// WebIDL §3.7.4 static operations.
///
/// ```ignore
/// #[v8_class]
/// impl Response {
///     #[v8_static_method]
///     fn json(scope: &mut v8::PinScope, value: v8::Local<v8::Value>)
///         -> Result<v8::Global<v8::Object>, OpError> { ... }
/// }
/// ```
///
/// Codegen skips the brand check, the internal-field deref, and the
/// `&self` / `&mut self` plumbing — static methods don't have a
/// receiver. The function is installed via `class_tmpl.set_with_attr`
/// so it shows up at `Class.method` (not on `Class.prototype.method`
/// or on instances).
///
/// Static methods cannot have a receiver (`&self` / `&mut self`); a
/// receiver triggers a `compile_error!` pointing at the method.
///
/// Outside a `#[v8_class]` impl block this attribute is a no-op.
#[proc_macro_attribute]
pub fn v8_static_method(_attr: TokenStream, item: TokenStream) -> TokenStream {
    item
}

/// Marker attribute consumed by `#[v8_class]`: declare a static getter
/// installed on the constructor function per WebIDL §3.7.4 static
/// attributes.
///
/// ```ignore
/// #[v8_class]
/// impl Box {
///     #[v8_static_getter]
///     fn DEFAULT_TIMEOUT() -> u32 { 5000 }
/// }
/// ```
///
/// Codegen emits the getter as a static method (no setter pairing),
/// installed via `set_accessor_property` on the constructor template.
/// The getter's body has no receiver and runs at every read of
/// `Class.DEFAULT_TIMEOUT`.
///
/// Outside a `#[v8_class]` impl block this attribute is a no-op.
#[proc_macro_attribute]
pub fn v8_static_getter(_attr: TokenStream, item: TokenStream) -> TokenStream {
    item
}

/// Marker attribute consumed by `#[v8_class]`: rename a method on the
/// JS-visible surface. `#[v8_name = "delete"]` lets a Rust `fn delete_`
/// be installed as `Foo.prototype.delete`. Outside of a `#[v8_class]`
/// impl block this is a no-op.
#[proc_macro_attribute]
pub fn v8_name(_attr: TokenStream, item: TokenStream) -> TokenStream {
    item
}

/// Impl-block-level marker attribute consumed by `#[v8_class]`:
/// override the default `Symbol.toStringTag` value. Without this, the
/// install codegen uses the Rust struct name (e.g.
/// `"HeadersIterator"`); with `#[v8_to_string_tag = "Headers Iterator"]`
/// it installs that literal instead. Used for WebIDL default iterator
/// objects whose spec tag is "<InterfaceName> Iterator".
#[proc_macro_attribute]
pub fn v8_to_string_tag(_attr: TokenStream, item: TokenStream) -> TokenStream {
    item
}

/// Impl-block-level marker attribute: chain the class's prototype to a
/// V8 built-in intrinsic. Currently the only recognised value is
/// `"IteratorPrototype"`, which sets the prototype's `[[Prototype]]`
/// to `%Iterator.prototype%` per WebIDL §3.7.10.2.
#[proc_macro_attribute]
pub fn v8_inherit_intrinsic(_attr: TokenStream, item: TokenStream) -> TokenStream {
    item
}

/// Impl-block-level marker attribute: chain the class's FunctionTemplate
/// to a base class's FunctionTemplate via `FunctionTemplate::inherit`.
/// Used for spec-mandated DOM-class inheritance, e.g.
/// `AbortSignal : EventTarget` (DOM §3.3) — without this the
/// `signal instanceof EventTarget === true` check fails.
///
/// Usage:
/// ```ignore
/// #[v8_class]
/// #[v8_inherit(EventTarget)]
/// impl AbortSignal { /* ... */ }
/// ```
///
/// Codegen calls `__ctor_tmpl.inherit(BaseClass::install(scope))` after
/// reserving internal-field slots; the base class's `install` is invoked
/// fresh per realm, which is fine because the template chain is per-
/// realm anyway. Per design fetch-native §XIV.1, this is the only
/// `#[v8_inherit]` user in v1; future users include WebSocket /
/// EventSource / MessagePort / XMLHttpRequest.
#[proc_macro_attribute]
pub fn v8_inherit(_attr: TokenStream, item: TokenStream) -> TokenStream {
    item
}

/// Impl-block-level marker attribute: project the JS-facing class
/// identity from a separate state struct. MAC-01 Phase 1.
///
/// Usage:
/// ```ignore
/// pub struct Request;                 // unit marker (JS class identity)
///
/// pub struct RequestState {           // boxed state in V8 internal field 0
///     /* RefCell<...> for every spec field */
/// }
///
/// #[v8_class]
/// #[v8_state_marker(Request)]
/// impl RequestState {
///     #[v8_constructor]
///     fn new(...) -> Result<RequestState, OpError> { /* ... */ }
///
///     #[v8_getter]
///     fn method(&self) -> String { /* &self is &RequestState */ }
/// }
/// ```
///
/// The marker drives JS-class identity: `set_class_name("Request")`,
/// install slot, brand check, callback names, `Symbol.toStringTag`. The
/// state drives the `Box<StateTy>` stored in V8 internal field 0 and
/// the per-method `&StateTy` / `&mut StateTy` receiver.
///
/// Without this attribute the macro behaves exactly as before (state
/// == marker == receiver). Adding it is opt-in and additive — no
/// existing class is affected.
///
/// See `docs/proposals/macro-v8-state.md` for the full design,
/// substitution table, and migration plan for `Request` / `Response`.
#[proc_macro_attribute]
pub fn v8_state_marker(_attr: TokenStream, item: TokenStream) -> TokenStream {
    item
}

/// `#[derive(WebIdlDict)]` — generate a `from_v8(scope, value) ->
/// Result<Self, OpError>` impl that reads the JS object's properties as
/// the struct's named fields, per WebIDL §3.10 (dictionaries).
///
/// Each field type must implement [`WebIdlConvertible`]. The trait is
/// hand-implemented for primitives (USVString, ByteString, String, bool,
/// u32, i32, f64), for `Option<T>` (lifts the blanket impl), for
/// `v8::Local<Value>` (passthrough for union-typed members), and is
/// auto-implemented by this derive AND by `#[derive(WebIdlEnum)]` on
/// user types. Dictionaries can therefore nest arbitrarily.
///
/// Override the WebIDL-visible member name with
/// `#[webidl_name = "..."]` on a field. Default = field ident verbatim.
///
/// Per spec, `null` / `undefined` produce a default-constructed dict
/// (the derive emits `Self::default()` — `Self: Default` is required).
/// Non-object values throw TypeError. Per-member conversion errors
/// throw with the inner converter's message.
///
/// # Per-field flags via `#[webidl_dict_member(...)]`
///
///   - `reject_null` — when the read value is JS `null`, throw
///     `TypeError` rather than fall through to `Default::default()`.
///     `undefined` and missing keys still default-construct (WebIDL
///     distinguishes the two). Used by
///     `AddEventListenerOptions.signal` per WebIDL §3.13.27 — the
///     nullable-AbortSignal contract is "MUST be a real AbortSignal
///     or absent — null is a TypeError".
///
/// See `crates/runtime-macros/src/webidl_dict.rs` for codegen detail.
#[proc_macro_derive(WebIdlDict, attributes(webidl_name, webidl_dict_member))]
pub fn webidl_dict_derive(input: TokenStream) -> TokenStream {
    webidl_dict::expand(input)
}

/// `#[derive(WebIdlEnum)]` — generate `from_str` / `as_str` / a
/// [`WebIdlConvertible`] impl for a unit-variant enum, per WebIDL
/// §3.7.10 (enumeration types).
///
/// By default each variant maps to its kebab-cased name (`NoCors` →
/// `"no-cors"`); override with `#[webidl_name = "..."]` on the variant.
///
/// The emitted [`WebIdlConvertible`] impl ToString-coerces the JS
/// value, runs `from_str`, and throws TypeError on unknown name (per
/// WebIDL §3.13.7 step 4). The error message includes both the
/// offending value and the accepted-name set.
///
/// The derive does NOT require `Self: Default` itself; the caller may
/// add `#[derive(Default)]` separately if WebIdlDict-as-member fallback
/// is needed.
///
/// # Type-level flags via `#[webidl_enum(...)]`
///
///   - `case_insensitive` — `from_str` and `from_v8` perform ASCII
///     case-insensitive matching against each variant's WebIDL name.
///     Used by WebCrypto `HashAlgo` per the spec normalisation rules
///     (`"SHA-256"` / `"sha-256"` / `"Sha-256"` are all valid).
///   - `silent_default` — `from_str` returns `Some(Self::default())`
///     on unknown name; `from_v8` ToStrings the value and returns
///     `Self::default()` on unknown rather than throwing TypeError.
///     **Requires `Self: Default`** (the derive emits a
///     `Self::default()` call). Used by Fetch / WebSocket spec sections
///     that explicitly tolerate unknown enum values (`RedirectMode`,
///     `CredentialsMode`, `BinaryType`).
///
/// Flags are combinable: `#[webidl_enum(silent_default, case_insensitive)]`.
///
/// See `crates/runtime-macros/src/webidl_enum.rs` for codegen detail.
#[proc_macro_derive(WebIdlEnum, attributes(webidl_name, webidl_enum))]
pub fn webidl_enum_derive(input: TokenStream) -> TokenStream {
    webidl_enum::expand(input)
}

/// Impl-block-level marker attribute consumed by `#[v8_class]`: emit
/// the WebIDL pair-iterator surface (keys / values / entries / forEach /
/// @@iterator) from a single user-supplied
/// `value_pairs(&self) -> Vec<(K, V)>` method.
///
/// The user's class must define a `value_pairs(&self)` method (without
/// the `#[v8_method]` marker — it stays Rust-private) that returns a
/// `Vec<(K, V)>`. The macro emits:
///
///   - `keys()` / `values()` / `entries()` factory methods (WebIDL
///     §3.7.10.2)
///   - `forEach(callback, thisArg?)` (WebIDL §3.7.10.3)
///   - `[Symbol.iterator]` aliasing `entries`
///   - A `<Class>Iterator` companion class with `next() -> { value, done }`
///
/// `K` and `V` must be one of: `ByteString`, `USVString`, `String`,
/// `u32`. Additionally `V` may be `Vec<u8>` (yielded as a Uint8Array).
///
/// **Iteration model**: the derive supports both snapshot and live
/// iteration via a `mode = ...` flag (default = `snapshot` for back-
/// compat):
///
///   - `mode = snapshot` (default): the iterator clones `value_pairs()`
///     once at factory-call time and walks the snapshot. Mutations to
///     the parent collection mid-iteration are NOT visible. Suits
///     read-only iterables (the common case).
///
///   - `mode = live`: each `next()` re-reads `value_pairs()` on the
///     parent and indexes at the current cursor; `forEach` re-reads
///     between callbacks. Mutations between yields ARE visible per
///     WebIDL §3.7.10.2. Required for Headers / FormData /
///     URLSearchParams iterators.
///
/// Usage:
/// ```ignore
/// struct MyMap { entries: Vec<(ByteString, ByteString)> }
///
/// #[v8_class]
/// #[v8_iterable(key = ByteString, value = ByteString)]
/// impl MyMap {
///     #[v8_constructor]
///     fn new() -> Self { ... }
///
///     fn value_pairs(&self) -> Vec<(ByteString, ByteString)> {
///         self.entries.clone()
///     }
/// }
///
/// // For collections whose contents can change mid-iteration:
/// struct Bag { entries: RefCell<Vec<(ByteString, ByteString)>> }
///
/// #[v8_class]
/// #[v8_iterable(key = ByteString, value = ByteString, mode = live)]
/// impl Bag {
///     // ... value_pairs reads self.entries.borrow().clone() ...
/// }
/// ```
#[proc_macro_attribute]
pub fn v8_iterable(_attr: TokenStream, item: TokenStream) -> TokenStream {
    item
}

/// Impl-block-level marker attribute consumed by `#[v8_class]`:
/// declare a WebIDL §3.7.5 interface constant. Repeatable — one
/// occurrence per constant.
///
/// ```ignore
/// #[v8_class]
/// #[v8_const(SYNTAX_ERR = 12u16)]
/// #[v8_const(NETWORK_ERR = 19u16)]
/// impl DOMException { ... }
/// ```
///
/// Each declaration installs the value at BOTH the constructor
/// function (`Class.NAME`) AND the prototype (`Class.prototype.NAME`)
/// per WebIDL §3.7.5, with a `{ writable: false, enumerable: true,
/// configurable: false }` descriptor (read-only, non-configurable;
/// enumerable per spec).
///
/// The literal's type-suffix selects how the value is materialised on
/// the V8 side:
///   - `u16` / `u32` → `v8::Integer::new_from_unsigned`
///   - `i32`         → `v8::Integer::new`
///
/// Unsuffixed literals or other type suffixes (`u64`, `i64`, `f64`,
/// etc.) are rejected with a `compile_error!`. Use `[Clamp]`-style
/// boundary newtypes for the runtime-side; constants are integers per
/// WebIDL §3.7.5.
///
/// Outside a `#[v8_class]` impl block this attribute is a no-op.
#[proc_macro_attribute]
pub fn v8_const(_attr: TokenStream, item: TokenStream) -> TokenStream {
    item
}

/// Impl-block-level marker attribute consumed by `#[v8_class]`: alias
/// `[Symbol.asyncIterator]` to a method that already exists on the
/// class, per WebIDL §3.7.10.5 (default async iterators).
///
/// Usage:
/// ```ignore
/// #[v8_class]
/// #[v8_async_iterable(method = "values")]
/// impl ReadableStream {
///     #[v8_method]
///     fn values(&self, ...) -> ... { ... }
/// }
/// ```
///
/// Codegen emits, in the install fn, a fresh FunctionTemplate wrapping
/// the named method's existing callback, with `set_class_name(method)`
/// per spec, and installs it on the prototype under
/// `Symbol.asyncIterator`. Identity isn't preserved (`obj[Symbol
/// .asyncIterator] !== obj.values`) — the spec describes two distinct
/// FunctionTemplates with matching callbacks and aligned `name`
/// properties, and consumers don't compare for identity.
///
/// Outside a `#[v8_class]` impl block this attribute is a no-op.
#[proc_macro_attribute]
pub fn v8_async_iterable(_attr: TokenStream, item: TokenStream) -> TokenStream {
    item
}

/// Argument-level marker attribute consumed by `#[v8_class]`: when
/// applied to a `Vec<u8>` parameter, the macro emits a SAB-rejection
/// guard *before* extracting the bytes. A SharedArrayBuffer-backed
/// view (or a bare `SharedArrayBuffer`) throws `TypeError` and the
/// method body never runs.
///
/// Per WebIDL §3.2.21: BufferSource arguments default to rejecting
/// shared backing stores; only the `[AllowShared]` extended attribute
/// opts in. We invert that for ergonomics — `Vec<u8>` extraction is
/// permissive by default (matches how legacy ops use it for blobs and
/// uploads), and the `#[reject_shared]` attribute pins down the spec-
/// strict cases (CompressionStream chunks, etc.).
///
/// Outside a `#[v8_class]` method param this attribute is a no-op.
#[proc_macro_attribute]
pub fn reject_shared(_attr: TokenStream, item: TokenStream) -> TokenStream {
    item
}

// ---------------------------------------------------------------------------
// Type helpers
// ---------------------------------------------------------------------------

// Wave 4 cleanup: type-classification helpers (is_unit_type, type_ident,
// is_vec_u8, is_vec_vec_u8, is_byte_string, is_enforce_range_u32 / _u64,
// is_usv_string, is_option_usv_string, first_generic_arg) moved to
// `types.rs`. The clamp_kind / wrap_kind standalone helpers from
// pre-Wave-4b folded into KnownType::Clamp(ClampInt) /
// KnownType::Wrap(WrapInt) (§3.7, closes F10/H8 + the stringly-typed
// dispatch anti-pattern).

// Wave 4 cleanup: argument-extraction + return-value codegen +
// V8-string + OpError-throw helpers all live in `codegen.rs` now.
// `gen_call_return`, `gen_extract`, `gen_extract_throw`, `must_str`,
// `must_str_abs`, `gen_throw_op_error_arms` are re-exported above for
// existing call sites in v8_class::emit and the WebIDL derives.
// The `Param` parameter struct lives in `types.rs`, re-exported above.
