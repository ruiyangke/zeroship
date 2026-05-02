# runtime-macros TODO

Status of macro extensions tracked here. Closed items keep their commit hash
so we can grep back through the rationale.

## Done

- **`#[v8_async_method]`** — async class methods compile to a sync V8
  callback that allocates a `v8::PromiseResolver`, spawns the user's
  body via `state.spawned_ops`, and returns the Promise. The pump
  resolves on `OpResult::JsValue`.
  - Compile-time guards: `&mut self` rejected (borrow-across-`.await`
    unsound under V8 re-entry); non-`async` fn rejected.
  - Supported return types: `()`, `bool`, `u32`, `i32`, `f64`,
    `String`, `Vec<u8>`, `v8::Global<v8::Value>`, plus `Result<T,
    OpError>` over any of the above.
  - Lands: commit `0a26d45b` (codegen + ResolveValue variants),
    `17b2e880` (16 smoke tests), `caa4f450` (compile-fail doctests).
  - Borrow safety: the future captures a `Global<v8::Object>` of the
    wrapper; as long as the future hasn't dropped, V8 cannot finalise
    the wrapper, so the boxed `Self` behind the recovered `*mut Self`
    stays valid across every poll. Detail in
    `gen_async_method_callback`'s doc comment.

## Open

### Brand check: every callback should verify `this` per WebIDL §3.7

**Source**: URL review, M4/M5 (2026-05-02). The local fix lives in
`crates/runtime/src/url_native/search_params.rs::is_url_search_params`.

#### Problem

The macro currently emits the following pattern for every method/getter/
setter callback (see `gen_method_callback` and `gen_setter_callback` in
`v8_class.rs`):

```rust
let __ext = match __this.get_internal_field(scope, 0)
    .and_then(|v| v8::Local::<v8::External>::try_from(v).ok())
{
    Some(e) => e,
    None => { /* throw "Illegal invocation" */ }
};
let __instance = unsafe { &mut *(__ext.value() as *mut #class_ty) };
```

The "verification" is "internal field 0 is an External" — but every
`#[v8_class]` instance with `internal_field_count = 1` has an External
there. So:

```js
URLSearchParams.prototype.entries.call(headers_instance)
//   the Headers Box is reinterpreted as a URLSearchParams Box → UB
```

WebIDL §3.7 mandates a real brand check: the receiver must be a genuine
instance of the class, verified by walking the prototype chain (or by
some other class-identity mechanism).

#### Local fix shipped (URLSearchParams)

`crates/runtime/src/url_native/search_params.rs` adds:
- `UrlNativeSlot::search_params_prototype: Global<Object>` — captured
  at install time.
- `is_url_search_params(obj, scope)` — walks the prototype chain
  (max depth 32) looking for the cached `URLSearchParams.prototype`.
- `iter_factory_callback` and `for_each_callback` invoke the check
  before any Box deref.

#### System-wide fix

Every `#[v8_class]` callback should perform an equivalent check before
the unsafe deref. Sketch:

1. At `install` time (in the macro-emitted code) cache the class's
   prototype in a per-class isolate slot (e.g.
   `__BrandSlot_<ClassTy>(Global<Object>)`).
2. In `gen_method_callback` / `gen_setter_callback` /
   `gen_constructor_callback`'s prologue, walk `__this`'s prototype
   chain for the cached prototype. If absent, throw "Illegal
   invocation".
3. The check is cheap (≤32 pointer comparisons; in practice 1–2 hops
   for a direct instance); the cost is dwarfed by the V8 callback
   overhead.

#### Affected classes (current map of `#[v8_class]`)

- Headers, Request, Response, FormData
- AbortSignal, EventTarget, Event
- Blob, File
- ReadableStream, ReadableStreamDefaultReader, etc. (all stream classes)
- URL, URLSearchParams, URLSearchParamsIterator (URLSearchParams +
  URLSearchParamsIterator already brand-checked locally; URL still
  relies on the unsafe pattern but has no cross-class lookalikes among
  installed `#[v8_class]` types).

#### Acceptance

```js
// Headers method called with non-Headers receiver throws
try { Headers.prototype.append.call({}, "k", "v"); }
catch (e) { /* expect TypeError "Illegal invocation" */ }

// Response method called with Request receiver throws (cross-class)
const req = new Request("https://x");
try { Response.prototype.text.call(req); }
catch (e) { /* expect TypeError */ }
```

### Same-name getter+setter pairing

Defining `#[v8_getter] value(&self)` and `#[v8_setter] value(&mut self, v)`
at once is illegal in Rust (duplicate method names) AND the install code
calls `set_accessor_property` separately for each. Fix needs a
`#[v8_name = "value"]` rename plus pairing in install codegen. Body's
`body`/`bodyUsed` are read-only so not blocking fetch.

### `#[reject_shared]` on `Vec<u8>` setter args

Currently only honoured on regular method args. Setters take exactly one
arg positionally, so the same logic should apply — but the
CompressionStream chunks path uses methods, not setters, so this
isn't blocking.

### Generic return type detection

`gen_call_return` currently handles a fixed list of scalar types
(bool/u32/i32/f64/String). New primitives (e.g. `u64` for byte-counter
getters) require a code edit in `lib.rs::gen_scalar_set`. A trait-based
dispatch (similar to `IntoResolveValue`) would let users opt in by
implementing the trait, but the existing list covers every fetch /
streams / WebSocket / WebCrypto consumer.

### `#[v8_constructor(must_new)]` — reject `Foo()` without `new`

WPT failures across event_target, blob_native, abort tests. V8's
FunctionTemplate doesn't expose a flag, but `args.is_construct_call()`
is queryable. Auto-emit the check at the top of the constructor
callback.

### `[NewObject]` semantic

WebIDL marker for getters that must return a fresh object per access
(`Response.json(data)`, future Crypto methods). Macro currently caches;
needs an opt-out attribute.

### `[Clamp]` integer coercion

WebIDL `[Clamp] long` clamps Number to integer range instead of
throwing. Used by Streams' chunk-size strategies and Blob.slice.
Currently hand-rolled via `f64::round_ties_even`. Add a `ClampLong`
newtype mirroring `EnforceRangeU64`.

### Reentrancy guard on `&mut self`

If a user-supplied JS callback re-enters the same instance, the macro's
auto-generated `borrow_mut` panics. Currently agents wrap state in
`RefCell` manually inside `Box<State>`. Macro could emit a soft
re-entry guard with a clear panic message, or auto-wrap in `RefCell`.

### Lifetime-tied `Local<'s, T>` returns

The synthetic `&mut PinScope` reborrow shortens the returned `Local`'s
lifetime. URL-native added a workaround: skip the reborrow when the
param is named `scope`. Streams hit it in different methods. Generalise:
detect when return type is `Local<'s, _>` tied to a scope arg and skip
the reborrow systematically.

### `#[derive(WebIdlDict)]` for dictionary parsing

RequestInit (16 members), ResponseInit, BlobPropertyBag, EventInit,
FilePropertyBag, QueuingStrategyInit, ReadableStreamGetReaderOptions,
etc. Each constructor today hand-rolls `obj.get(scope, key)` calls. A
derive would emit:

```rust
#[derive(WebIdlDict)]
struct RequestInit {
    method: Option<USVString>,
    body: Option<v8::Local<v8::Value>>,
    headers: Option<HeadersInit>,
    signal: Option<v8::Local<v8::Object>>,
    // ...
}
```

Saves ~40 LOC per constructor; consistent error messages on bad members.

### `#[derive(WebIdlEnum)]`

WebIDL enums (RequestMode, RequestCache, RequestRedirect,
RequestCredentials, RequestDestination, ReferrerPolicy, ResponseType,
ReadableStreamReaderMode, ReadableStreamType, …). Currently hand-rolled
per class. Derive would emit `from_str` + WebIDL-correct unknown-value
rejection.

### `#[v8_iterable(key=K, value=V)]` derive

Headers, FormData, URLSearchParams each ship ~150 LOC of
structurally-identical iterator boilerplate. A derive could emit all of
it from a single `value_pairs(&self) -> &[(K, V)]` method. Net ~400 LOC
removed across the 5 iterable classes.

### `[SameObject]` cache attribute

`Request.headers`, `URL.searchParams`, several others must return the
same object reference across accesses. Each impl caches via a V8 private
symbol. A `#[v8_getter(same_object)]` attribute would emit the cache
automatically.

### V8 fastcall annotation — `#[v8_getter(fastcall)]` / `#[v8_method(fastcall)]`

Turbofan can inline `CFunction` callbacks at hot call sites,
skipping External lookup, scope setup, and the FunctionCallback
entry/exit dance. ~10–30 ns saved per inlined call.

**Macro shape:** an attribute that emits BOTH a slow-path
`FunctionCallback` (current behavior, unchanged) AND a typed
`CFunction` shim, then wires them via
`function_template.set_c_function(...)`. User's Rust fn must be
`extern "C"` and accept a final `*mut FastApiCallbackOptions` arg
so it can opt into the slow-path fallback on edge cases (multibyte
strings, exception paths, etc.).

**Constraints (V8-imposed):**
- No allocation (no new JS objects, no GC).
- No exceptions in the fast path — set
  `FastApiCallbackOptions::fallback = true` to bail to slow path.
- Restricted arg/return types: `i32` / `u32` / `i64` / `u64` /
  `f32` / `f64` / `bool`, plus `Local<Value>`, `FastOneByteString`
  (ASCII string fast path), and `FastApiTypedArray<T>`. WebIDL
  `DOMString` requires `FastOneByteString` + slow-path fallback
  on multibyte.

**Top ROI candidates (ordered by call frequency × per-call savings):**

| Site | Signature | Why hot |
|---|---|---|
| `AbortSignal.aborted` getter | `(this) → bool` | Every cancel-aware op checks; ~20 ns × N/req |
| `URL.protocol` / `.host` / `.pathname` getters | `(this) → FastOneByteString` | User handlers parsing URLs |
| `Headers.has(name)` | `(this, FastOneByteString) → bool` | Routing / proxy handlers |
| `crypto.getRandomValues(buf)` | `(this, FastApiTypedArray<u8>) → void` | Currently allocates + copies; fastcall writes in place |
| `Streams.desiredSize` getter | `(this) → f64` | Backpressure-aware producers |
| `URLSearchParams.size` getter | `(this) → u32` | Common in dispatch logic |

**Phasing:** start with `AbortSignal.aborted` + `URL.pathname`
(highest call frequency in real handlers), measure with the bench
harness against the v8-1w slot (`--scenario=httpGet --duration=5s`).
Expand if a single attribute saves ≥5%; pause if not.

### Better compile errors

Today: "#[v8_class] requires a plain type" is the only structured
error. Most user mistakes (wrong receiver, missing `#[v8_method]`,
wrong Result return) surface as cryptic syn errors. Worth a 50-LOC
investment in friendly diagnostics for common shapes.
