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
use proc_macro2::TokenStream as TokenStream2;
use quote::quote;
use syn::{GenericArgument, Ident, PathArguments, ReturnType, Type, TypePath};

mod known_type;
mod v8_class;
mod v8_iterable;
mod webidl_dict;
mod webidl_enum;

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

/// True for the unit type `()`. Returned by mutator-style methods
/// like `Result<(), OpError>` that have nothing to set on `rv` —
/// they want the JS-visible call to evaluate to `undefined`.
pub(crate) fn is_unit_type(ty: &Type) -> bool {
    matches!(ty, Type::Tuple(t) if t.elems.is_empty())
}

/// Extract the last segment identifier from a type path (e.g. `String`, `Option`, `Result`).
pub(crate) fn type_ident(ty: &Type) -> Option<String> {
    if let Type::Path(TypePath { path, .. }) = ty {
        path.segments.last().map(|s| s.ident.to_string())
    } else {
        None
    }
}

/// Check if type is `Vec<u8>` — used for binary data args (reads from ArrayBufferView).
pub(crate) fn is_vec_u8(ty: &Type) -> bool {
    type_ident(ty).as_deref() == Some("Vec")
        && first_generic_arg(ty)
            .and_then(type_ident)
            .as_deref()
            == Some("u8")
}

/// Check if type is `Vec<Vec<u8>>` — used by IDL methods like
/// `getSetCookie() -> sequence<ByteString>`. Marshalled as a JS Array
/// of ByteString (each element is a Latin-1 one-byte string).
pub(crate) fn is_vec_vec_u8(ty: &Type) -> bool {
    type_ident(ty).as_deref() == Some("Vec")
        && first_generic_arg(ty)
            .map(is_vec_u8)
            .unwrap_or(false)
}

/// Check if type is the `ByteString` newtype from
/// `zeroship_runtime::byte_string`. Used for WebIDL ByteString args
/// (Headers names/values etc.). Detection is by last segment ident; we
/// don't enforce the full path since users typically `use
/// ::zeroship_runtime::byte_string::ByteString` or alias the type.
pub(crate) fn is_byte_string(ty: &Type) -> bool {
    type_ident(ty).as_deref() == Some("ByteString")
}

/// Check if type is the `EnforceRangeU64` newtype from
/// `zeroship_runtime::enforce_range`. Used for WebIDL `[EnforceRange]
/// unsigned long long` args (BYOBReader.read min, BYOBRequest.respond
/// bytesWritten). Detection by last segment ident, like ByteString.
pub(crate) fn is_enforce_range_u64(ty: &Type) -> bool {
    type_ident(ty).as_deref() == Some("EnforceRangeU64")
}

/// Check if type is the `EnforceRangeU32` newtype — companion to
/// `EnforceRangeU64` for WebIDL `[EnforceRange] unsigned long`.
/// Used by the WebCrypto IDL surface (Pbkdf2Params.iterations,
/// RsaKeyGenParams.modulusLength, deriveBits.length, etc.).
/// See `docs/proposals/webcrypto-native.md` D-20.
pub(crate) fn is_enforce_range_u32(ty: &Type) -> bool {
    type_ident(ty).as_deref() == Some("EnforceRangeU32")
}

// Wave 4b: `clamp_kind` / `wrap_kind` standalone helpers folded into
// `KnownType::Clamp(ClampInt)` / `KnownType::Wrap(WrapInt)` (closes the
// stringly-typed-dispatch anti-pattern §3, plus F10/H8). The variant
// data IS the suffix; no "unrecognised suffix" `unreachable!` arm to
// audit.

/// Check if type is the `USVString` newtype from
/// `zeroship_runtime::url_native::helpers`. Used for WebIDL USVString
/// args (URL.* setters, URLSearchParams names/values). Conversion
/// replaces unmatched surrogate code units with U+FFFD.
pub(crate) fn is_usv_string(ty: &Type) -> bool {
    type_ident(ty).as_deref() == Some("USVString")
}

/// Check if type is `Option<USVString>`.
pub(crate) fn is_option_usv_string(ty: &Type) -> bool {
    type_ident(ty).as_deref() == Some("Option")
        && first_generic_arg(ty).map(is_usv_string).unwrap_or(false)
}

/// Extract the first generic type argument (e.g. `String` from `Option<String>`).
pub(crate) fn first_generic_arg(ty: &Type) -> Option<&Type> {
    if let Type::Path(TypePath { path, .. }) = ty {
        if let Some(seg) = path.segments.last() {
            if let PathArguments::AngleBracketed(ref ab) = seg.arguments {
                for arg in &ab.args {
                    if let GenericArgument::Type(t) = arg {
                        return Some(t);
                    }
                }
            }
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Parameter parsing
// ---------------------------------------------------------------------------

pub(crate) struct Param {
    pub(crate) name: Ident,
    pub(crate) ty: Type,
}

// ---------------------------------------------------------------------------
// Argument extraction codegen (JS value → Rust type)
// ---------------------------------------------------------------------------

/// Emit `v8::String::new(<scope>, <lit>).unwrap()` for a string literal
/// or interpolated str token.
///
/// The 50+ call sites of this pattern across the crate's emit code are
/// noisy — `v8::String::new` returns `Option<Local<String>>` and is
/// `None` only on V8 string-pool exhaustion (a near-zero probability
/// event in practice; V8 itself aborts on isolate OOM well before
/// this), so every site does an `.unwrap()`. Centralising the pattern:
///   - Reduces visual noise in the generated code's templates.
///   - Gives one place to switch to a panic-free fallback if we ever
///     decide to surface OOM as a V8 RangeError instead of aborting.
///   - Makes drift easier to spot — if a future change wants the
///     `new_from_onebyte_const` ASCII fast path, it's one helper edit
///     instead of 50 grep-and-replace sites.
///
/// `scope_expr` is interpolated as the V8 scope binding (almost always
/// `scope` in callbacks; the parameter form lets factory codegen pass
/// a different binding without rebinding). `lit` is interpolated
/// directly — pass either a string literal (`"prototype"`) or a
/// pre-built token stream that names a `&str` binding (a `String` /
/// `&str` ident, etc.).
///
/// Output token shape (byte-identical to the previous open-coded
/// pattern, verified by the v8_class snapshot suite):
///
/// ```ignore
/// v8::String::new(<scope>, <lit>).unwrap()
/// ```
///
/// Use [`must_str_abs`] when the surrounding emit code uses the
/// absolute `::v8::` path (e.g. derive macros' emit, where the user's
/// crate may not have `use v8;` imported).
///
/// # Wave-1 scope
///
/// This Wave-1 sweep migrates the call sites in `lib.rs`, `mod.rs`,
/// `webidl_dict.rs`, `webidl_enum.rs`. The remaining sites in
/// `method.rs` and `v8_iterable.rs` are owned by Wave 1 #170 / #171
/// and will pick up the helper as part of those merges.
pub(crate) fn must_str(scope_expr: &TokenStream2, lit: &TokenStream2) -> TokenStream2 {
    quote! { v8::String::new(#scope_expr, #lit).unwrap() }
}

/// Absolute-path variant of [`must_str`]. Emits
/// `::v8::String::new(<scope>, <lit>).unwrap()`. Used by the WebIDL
/// derive macros, whose emit lives in user crates that may not have
/// imported `v8` directly.
pub(crate) fn must_str_abs(scope_expr: &TokenStream2, lit: &TokenStream2) -> TokenStream2 {
    quote! { ::v8::String::new(#scope_expr, #lit).unwrap() }
}

/// Single source of truth for the OpError → V8 exception dispatch. Emits
/// the full 6-variant match (TypeError, RangeError, DomException,
/// NodeError, Error, JsValue passthrough) used wherever the macro
/// translates a `Result<T, OpError>` boundary into a JS `throw`.
///
/// `scope_expr` and `err_expr` are inserted as the V8 scope and the
/// `OpError` reference, respectively — typically `quote!(scope)` and
/// `quote!(__err)` in slow-path callbacks. They're parametric so the
/// helper can be re-used from sites that bind these under different
/// names (e.g. async-method post-resolution). The emitted block:
///
/// ```ignore
/// if let OpErrorKind::JsValue(g) = &<err>.kind {
///     <scope>.throw_exception(Local::new(<scope>, g));
/// } else {
///     let msg = v8::String::new(<scope>, &<err>.message).unwrap();
///     let exc = match &<err>.kind {
///         OpErrorKind::TypeError       => v8::Exception::type_error(<scope>, msg),
///         OpErrorKind::RangeError      => v8::Exception::range_error(<scope>, msg),
///         OpErrorKind::DomException(n) => zeroship::dom::exception::build(<scope>, &<err>.message, n).into(),
///         OpErrorKind::NodeError(c)    => zeroship::node_error::build_node_exception(<scope>, c, &<err>.message),
///         OpErrorKind::Error           => v8::Exception::error(<scope>, msg),
///         OpErrorKind::JsValue(_)      => unreachable!(),
///     };
///     <scope>.throw_exception(exc);
/// }
/// ```
///
/// Callers MUST emit `return;` (or whatever control-flow primitive
/// suits the surrounding callback shape) AFTER this block — the helper
/// only produces the exception-throw, never the unwind.
///
/// # Migration note
///
/// As of this commit, two of the four historical OpError-throw sites
/// in this crate route through this helper:
///   - `gen_extract_throw` (per-arg extraction failures) — was missing
///     DomException/NodeError, now covers all 6 variants.
///   - `gen_throw_error` (call-return Result arm).
///
/// The remaining two sites — both in `v8_class/method.rs` — are owned
/// by Wave 1 #170 (statics agent) and Wave 1 #171 (iterators agent)
/// respectively. They will pick up this helper as part of their merge.
/// Until then, a `__msg` binding mismatch with `__err.kind`-without-a-
/// `&` deref in the post_init arm is preserved verbatim in those files.
pub(crate) fn gen_throw_op_error_arms(
    scope_expr: &TokenStream2,
    err_expr: &TokenStream2,
) -> TokenStream2 {
    let msg_init = must_str(scope_expr, &quote! { &(#err_expr).message });
    quote! {
        if let ::zeroship_runtime::state::OpErrorKind::JsValue(__global) = &(#err_expr).kind {
            let __local = v8::Local::new(#scope_expr, __global);
            (#scope_expr).throw_exception(__local);
        } else {
            let __msg = #msg_init;
            let __exc: v8::Local<v8::Value> = match &(#err_expr).kind {
                ::zeroship_runtime::state::OpErrorKind::TypeError => v8::Exception::type_error(#scope_expr, __msg),
                ::zeroship_runtime::state::OpErrorKind::RangeError => v8::Exception::range_error(#scope_expr, __msg),
                ::zeroship_runtime::state::OpErrorKind::DomException(__name) => {
                    ::zeroship_runtime::dom::exception::build(#scope_expr, &(#err_expr).message, __name).into()
                }
                ::zeroship_runtime::state::OpErrorKind::NodeError(__code) => {
                    ::zeroship_runtime::node_error::build_node_exception(#scope_expr, __code, &(#err_expr).message)
                }
                ::zeroship_runtime::state::OpErrorKind::Error => v8::Exception::error(#scope_expr, __msg),
                // Already handled by the early-return above.
                ::zeroship_runtime::state::OpErrorKind::JsValue(_) => unreachable!(),
            };
            (#scope_expr).throw_exception(__exc);
        }
    }
}

/// Emit the throw machinery for an `OpError` named `__err` in scope.
/// Used by the per-arg-extraction codegen (ByteString / USVString /
/// EnforceRange / etc.) where a conversion failure must surface as a
/// V8 exception and `return` from the V8 callback. The macro emits
/// `return` after this snippet — that's the caller's responsibility.
///
/// Pre-2026-05-05 this used a 3-arm match that downgraded
/// DomException/NodeError to a generic `Error`; that was a contract
/// bug — extraction can return any OpError variant via dict / enum
/// `WebIdlConvertible::from_v8`, and a `OpError::dom_exception(...)`
/// MUST surface as a real `DOMException`, not a generic `Error`. Now
/// delegates to [`gen_throw_op_error_arms`] for the full 6-variant
/// match, in lockstep with `gen_throw_error`.
pub(crate) fn gen_extract_throw() -> TokenStream2 {
    let scope = quote! { scope };
    let err = quote! { __err };
    gen_throw_op_error_arms(&scope, &err)
}

/// Emit the per-arg extraction tokens for the slow-path
/// FunctionCallback. Wave 4b — delegates to the table-driven
/// [`KnownType`] classifier (design §3.7, closes F10 / H8). The body
/// here is a thin shim: classify the type once, ask the variant for
/// its emission. The 13-arm string-keyed dispatch and the
/// `clamp_kind` / `wrap_kind` standalone helpers (with their
/// `unreachable!` arms) used to live inline; they fold into
/// `KnownType::extract_tokens` and the variant data, respectively.
pub(crate) fn gen_extract(index: usize, name: &Ident, ty: &Type) -> TokenStream2 {
    let idx = index as i32;
    known_type::KnownType::from_ty(ty).extract_tokens(name, idx)
}

// ---------------------------------------------------------------------------
// Return value codegen (Rust value → V8 value)
// ---------------------------------------------------------------------------

/// Generate code to write Vec<u8> as a V8 Uint8Array.
///
/// Returning a plain ArrayBuffer was easier but spec-wrong for every
/// real consumer: WHATWG TextEncoder.encode and WebCrypto digest both
/// return Uint8Array, and downstream JS code (streams, fetch body
/// coercion) typically branches on `instanceof Uint8Array` to decide
/// whether to wrap. Returning Uint8Array matches the spec contract
/// without forcing every caller to do `new Uint8Array(arrayBuffer)`.
fn gen_vec_u8_set(val: &TokenStream2) -> TokenStream2 {
    quote! {
        let __bytes = #val;
        let __len = __bytes.len();
        let __ab = v8::ArrayBuffer::new(scope, __len);
        let __store = __ab.get_backing_store();
        for (__i, &__b) in __bytes.iter().enumerate() {
            __store[__i].set(__b);
        }
        let __u8 = v8::Uint8Array::new(scope, __ab, 0, __len).unwrap();
        rv.set(__u8.into());
    }
}

/// Generate code to convert a scalar value (referenced by `val` tokens) to a V8 return value.
fn gen_scalar_set(ty: &Type, val: &TokenStream2) -> TokenStream2 {
    if is_vec_u8(ty) {
        return gen_vec_u8_set(val);
    }
    // `v8::Local<v8::Value>` (or any v8::Local<v8::T>): pass straight
    // to `rv.set`. Used by methods that build their own JS object,
    // typed array, etc. — e.g. TextEncoder.encodeInto returning
    // `{ read, written }`.
    if type_ident(ty).as_deref() == Some("Local") {
        return quote! { rv.set(#val.into()); };
    }
    match type_ident(ty).as_deref() {
        Some("bool") => quote! { rv.set(v8::Boolean::new(scope, #val).into()); },
        Some("u32") => quote! { rv.set(v8::Integer::new_from_unsigned(scope, #val).into()); },
        Some("i32") => quote! { rv.set(v8::Integer::new(scope, #val).into()); },
        Some("f64") => quote! { rv.set(v8::Number::new(scope, #val).into()); },
        // Default: String
        _ => {
            let scope = quote! { scope };
            let v_init = must_str(&scope, &quote! { &#val });
            quote! {
                let __v = #v_init;
                rv.set(__v.into());
            }
        }
    }
}

/// Generate code to convert an `Option<T>` inner value to V8. The
/// emitted code expects `__inner` to be the unwrapped Some value.
fn gen_option_some_set(ty: &Type) -> TokenStream2 {
    if is_vec_u8(ty) {
        // Option<Vec<u8>> for getters like `Headers.get(name) ->
        // ByteString?`. Emit a Latin-1 one-byte string so byte fidelity
        // is preserved (encoded session tokens, e.g. high-bit Set-Cookie
        // values, must round-trip).
        return quote! {
            let __v = v8::String::new_from_one_byte(
                scope,
                __inner.as_slice(),
                v8::NewStringType::Normal,
            ).unwrap();
            rv.set(__v.into());
        };
    }
    match type_ident(ty).as_deref() {
        Some("bool") => quote! { rv.set(v8::Boolean::new(scope, __inner).into()); },
        Some("u32") => quote! { rv.set(v8::Integer::new_from_unsigned(scope, __inner).into()); },
        _ => {
            let scope = quote! { scope };
            let v_init = must_str(&scope, &quote! { &__inner });
            quote! {
                let __v = #v_init;
                rv.set(__v.into());
            }
        }
    }
}

/// Generate code to build a `v8::Array` from a `Vec<String>`.
fn gen_vec_set() -> TokenStream2 {
    let scope = quote! { scope };
    let v_init = must_str(&scope, &quote! { __s });
    quote! {
        let __arr = v8::Array::new(scope, __vec.len() as i32);
        for (__i, __s) in __vec.iter().enumerate() {
            let __v = #v_init;
            __arr.set_index(scope, __i as u32, __v.into());
        }
        rv.set(__arr.into());
    }
}

/// Generate code to build a `v8::Array` from a `Vec<Vec<u8>>`. Each
/// element is materialised as a Latin-1 one-byte string (a WebIDL
/// ByteString round-trips faithfully — bytes 0x80–0xFF survive). Used
/// by methods like `Headers.getSetCookie() -> sequence<ByteString>`.
fn gen_vec_vec_u8_set() -> TokenStream2 {
    quote! {
        let __arr = v8::Array::new(scope, __vec.len() as i32);
        for (__i, __bytes) in __vec.iter().enumerate() {
            let __s = v8::String::new_from_one_byte(
                scope,
                __bytes.as_slice(),
                v8::NewStringType::Normal,
            ).unwrap();
            __arr.set_index(scope, __i as u32, __s.into());
        }
        rv.set(__arr.into());
    }
}

/// Generate error throw from `OpError`.
///
/// - `OpErrorKind::JsValue(global)` re-throws the captured user-thrown
///   value verbatim — preserves Error subclass identity, custom
///   properties (e.g. `e.code`), and the `instanceof` chain. Used by
///   the dict / enum derives' per-member tc-scope path: when a
///   member's `WebIdlConvertible::from_v8` triggers user JS that
///   throws (custom `toString`, throwing `Symbol.toPrimitive`), the
///   captured exception value MUST reach the caller's `catch` block
///   unchanged.
/// - `OpErrorKind::DomException(name)` constructs a real DOMException
///   instance via `new globalThis.DOMException(message, name)`. The
///   native DOMException class is installed during `setup_globals` (see
///   `crates/runtime/src/dom/exception.rs`); the constructor lookup is
///   per-throw because callers of this codegen don't always have the
///   active class function in scope. Per `docs/proposals/webcrypto-native.md`
///   D-6.
/// - `OpErrorKind::NodeError(code)` constructs a JS Error / TypeError /
///   RangeError per the per-code class table (see
///   `core/node_error.rs::class_for`) and assigns the `code` property
///   for `if (e.code === "ERR_...")` branching. Per
///   `docs/proposals/node-crypto-native.md` D-N32.
fn gen_throw_error() -> TokenStream2 {
    // Routed through the shared `gen_throw_op_error_arms` helper —
    // single source of truth for the 6-variant OpErrorKind dispatch.
    // Adding a 7th variant means editing one match in one helper.
    let scope = quote! { scope };
    let err = quote! { __err };
    gen_throw_op_error_arms(&scope, &err)
}

/// Generate the function call + return value handling.
///
/// `call` is the pre-built call expression (e.g. `__instance.method(a,
/// b)`). Used by `#[v8_class]` codegen to marshal whatever the user's
/// method returned into the V8 `ReturnValue`.
pub(crate) fn gen_call_return(call: &TokenStream2, output: &ReturnType) -> TokenStream2 {
    match output {
        ReturnType::Default => quote! { #call; },
        ReturnType::Type(_, ty) => {
            let outer = type_ident(ty);
            match outer.as_deref() {
                // --- Result<T, OpError> ---
                Some("Result") => {
                    let inner = first_generic_arg(ty);
                    let inner_ident = inner.and_then(type_ident);
                    let ok_handling = if inner.map(is_unit_type).unwrap_or(false) {
                        // `Result<(), OpError>` — Ok variant has no value
                        // to surface. Bind it to `_` so the unused-let
                        // lint doesn't fire, and leave `rv` untouched
                        // (defaults to `undefined`). Used by mutator
                        // methods like Headers.append.
                        quote! { let _ = __ok; }
                    } else {
                        match inner_ident.as_deref() {
                            Some("Option") => {
                                let inner2 = inner.and_then(first_generic_arg);
                                let some_set = inner2
                                    .map(gen_option_some_set)
                                    .unwrap_or_else(|| gen_option_some_set(&syn::parse_quote!(String)));
                                quote! {
                                    match __ok {
                                        Some(__inner) => { #some_set }
                                        None => rv.set(v8::null(scope).into()),
                                    }
                                }
                            }
                            Some("Vec") => {
                                if inner.map(is_vec_u8).unwrap_or(false) {
                                    let val = quote! { __ok };
                                    gen_vec_u8_set(&val)
                                } else if inner.map(is_vec_vec_u8).unwrap_or(false) {
                                    let vv_set = gen_vec_vec_u8_set();
                                    quote! {
                                        let __vec = __ok;
                                        #vv_set
                                    }
                                } else {
                                    let vec_set = gen_vec_set();
                                    quote! {
                                        let __vec = __ok;
                                        #vec_set
                                    }
                                }
                            }
                            _ => {
                                let val = quote! { __ok };
                                inner
                                    .map(|t| gen_scalar_set(t, &val))
                                    .unwrap_or_else(|| gen_scalar_set(&syn::parse_quote!(String), &val))
                            }
                        }
                    };
                    let throw = gen_throw_error();
                    quote! {
                        match #call {
                            Ok(__ok) => { #ok_handling }
                            Err(__err) => { #throw }
                        }
                    }
                }

                // --- Option<T> ---
                Some("Option") => {
                    let inner = first_generic_arg(ty);
                    let some_set = inner
                        .map(gen_option_some_set)
                        .unwrap_or_else(|| gen_option_some_set(&syn::parse_quote!(String)));
                    quote! {
                        match #call {
                            Some(__inner) => { #some_set }
                            None => rv.set(v8::null(scope).into()),
                        }
                    }
                }

                // --- Vec<T> ---
                Some("Vec") => {
                    if is_vec_u8(ty) {
                        let val = quote! { __vec };
                        let ab_set = gen_vec_u8_set(&val);
                        quote! {
                            let __vec = #call;
                            #ab_set
                        }
                    } else if is_vec_vec_u8(ty) {
                        let vv_set = gen_vec_vec_u8_set();
                        quote! {
                            let __vec = #call;
                            #vv_set
                        }
                    } else {
                        let vec_set = gen_vec_set();
                        quote! {
                            let __vec = #call;
                            #vec_set
                        }
                    }
                }

                // --- Scalars ---
                //
                // Bind the user-method call's result to a local FIRST,
                // then construct the V8 value. Inlining `#call` into
                // `v8::Integer::new_from_unsigned(scope, #call)` would
                // borrow `scope` twice in the same expression — once
                // immutably for the first arg, once mutably inside
                // `#call` (when the user method itself takes
                // `scope: &mut PinScope`). E0502.
                Some("bool") => quote! {
                    let __r = #call;
                    rv.set(v8::Boolean::new(scope, __r).into());
                },
                Some("u32") => quote! {
                    let __r = #call;
                    rv.set(v8::Integer::new_from_unsigned(scope, __r).into());
                },
                Some("i32") => quote! {
                    let __r = #call;
                    rv.set(v8::Integer::new(scope, __r).into());
                },
                Some("f64") => quote! {
                    let __r = #call;
                    rv.set(v8::Number::new(scope, __r).into());
                },
                Some("String") => {
                    let scope = quote! { scope };
                    let v_init = must_str(&scope, &quote! { &__r });
                    quote! {
                        let __r = #call;
                        let __v = #v_init;
                        rv.set(__v.into());
                    }
                }

                // --- Direct V8 value (Local<Value>, Local<Object>, etc.) ---
                // Used by methods that build a custom JS shape (e.g.
                // TextEncoder.encodeInto returning `{ read, written }`).
                Some("Local") => quote! { rv.set(#call.into()); },

                _ => quote! { #call; },
            }
        }
    }
}
