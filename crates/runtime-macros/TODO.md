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

- **Brand check via cached prototype walk (WebIDL §3.7)** — every
  method/getter/setter/async-method callback now walks `this`'s
  prototype chain looking for the cached `Foo.prototype`, throwing
  `TypeError("Illegal invocation")` synchronously before the unsafe
  internal-field deref. Pre-fix the only check was "internal field 0
  is an External", which let cross-class deception
  (`Foo.prototype.method.call(bar)`) reinterpret a Bar Box as a Foo
  Box and dereference — UB whenever the structs diverged in field
  layout, and reachable from any user JS.
  - Capture: `install` snapshots `Foo.prototype` as a Global<Object>
    in a `__BrandSlot_<ClassTy>` isolate slot, after any
    `#[v8_inherit]` / `#[v8_inherit_intrinsic]` chaining has settled.
  - Check: per-class `__brand_check_<ClassTy>(scope, obj)` walks up
    to 32 prototype links comparing handle identity against the
    cached prototype. Subclasses (via `#[v8_inherit]`) match because
    the parent prototype IS on their chain.
  - Cost: 1–3 extra Local pointer comparisons per call (typical
    chain depth), dwarfed by V8's ~100ns callback overhead.
  - Lands: commit `c95915e1` (macro codegen + 7 smoke tests in
    `tests/v8_brand_check_smoke.rs`).
  - The local fix in `url_native/search_params.rs::is_url_search_params`
    is now redundant for any class going through the macro; it's left
    in place as the URL-specific manual brand check until that file
    is migrated.

- **`#[v8_constructor(must_new)]` — reject `Foo()` without `new`**
  (WebIDL §3.7.1). Default-on for every macro-emitted constructor
  (both user-supplied `#[v8_constructor]` and the Default-derived
  fallback). Pre-fix, calling `Foo()` (no `new`) bound `this` to
  globalThis and let the constructor write internal fields onto the
  wrong shape. WPT failures across event_target, blob_native, and
  abort all stem from this gap.
  - The TypeError message interpolates the class name so callers
    diagnose mistakes per-class
    (`"Failed to construct 'Headers': Please use the 'new' operator…"`).
  - Opt-out via `#[v8_constructor(callable_no_new)]` for future
    legacy-callable WebIDL shapes (none today; the attribute is
    parsed and threaded but no class uses it).
  - Lands: commit `5b16b016` (macro prologue + 5 smoke tests in
    `tests/v8_must_new_smoke.rs`).

- **`[SameObject]` getter cache attribute** — `#[v8_getter(same_object)]`
  wraps a getter with WebIDL [SameObject] caching: subsequent reads
  on the same wrapper instance return the same JS Object via a V8
  Private symbol stash, instead of minting a fresh Object on every
  read.
  - Cache key: per-class-and-getter Private symbol
    `__zs_same_object_<ClassTy>_<getter>` on the wrapper instance.
  - User method returns `v8::Global<v8::Object>` (minted on first
    call); macro stashes via `set_private` and returns the cached
    Local thereafter. Brand check still applies on the cached path.
  - Existing hand-rolled SameObject implementations
    (`Request.headers` in `fetch_request.rs:299`, `Response.headers`,
    `URL.searchParams`) are NOT migrated in the same commit — that's
    a follow-up. The smoke test proves the attribute works.
  - Lands: commit `3cb0fe11` (codegen + 4 smoke tests in
    `tests/v8_same_object_smoke.rs`).

## Open

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

### Migrate hand-rolled `[SameObject]` getters to `#[v8_getter(same_object)]`

The macro now ships the attribute (commit `3cb0fe11`) but the existing
hand-rolled SameObject implementations (`Request.headers` in
`fetch_request.rs:299`, `Response.headers`, `URL.searchParams`) still
hand-roll the V8 Private symbol stash. Migrate each to the attribute
to delete the boilerplate and keep one cache implementation in the
codebase.

### V8 fastcall annotation

Turbofan can inline FastApiCall callbacks, skipping the External read
on hot paths. Candidates: Headers `.has()` / `.get()`, Streams
`.desiredSize` getter, URL component getters. ~10–30ns/call on inlined
paths.

### Better compile errors

Today: "#[v8_class] requires a plain type" is the only structured
error. Most user mistakes (wrong receiver, missing `#[v8_method]`,
wrong Result return) surface as cryptic syn errors. Worth a 50-LOC
investment in friendly diagnostics for common shapes.
