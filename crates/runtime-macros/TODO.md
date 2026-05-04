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
    `tests/v8_brand_check_smoke.rs`); follow-up `b0339e23` makes the
    prototype capture lazy on first brand check (eager get_function
    inside `install` froze the FunctionTemplate's instance shape and
    silently no-op'd late accessor installs like URL.searchParams).
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

- **`[NewObject]` semantic — confirmed: default IS no-cache** (audit
  only, no codegen change). The TODO entry implied the macro was
  caching default getter results and asked for an opt-out attribute.
  Reading `gen_method_callback` (the path every non-`same_object`
  getter takes) shows the user method runs unconditionally on each
  read and `gen_call_return` sets `rv` directly — no Private-symbol
  stash, no instance-scoped cache. The implicit default IS therefore
  WebIDL `[NewObject]`. Caching is the OPT-IN: `#[v8_getter(same_object)]`
  (commit `3cb0fe11`). No new attribute required.
  - Smoke test `tests/v8_new_object_smoke.rs` (2 tests) demonstrates
    that a default `#[v8_getter]` returning `v8::Local<v8::Value>`
    mints a fresh JS Object on every read (`a !== b`) and runs the
    user method N times for N reads. Pairs with the existing
    `tests/v8_same_object_smoke.rs` to document both halves of the
    contract.
  - Lands: commit `4a557dc4` (smoke test only, no codegen delta).

- **Re-entry guard on `&mut self`** — every macro-emitted `&mut self`
  callback (regular method, setter, SameObject getter cache-miss path)
  now opens with a per-method, per-instance, thread-local
  `RefCell<HashSet<usize>>` guard keyed by the External pointer's
  address (`__ext.value() as usize` == Box raw addr). On entry: insert.
  If the addr was already in the set, throw a V8 TypeError with a
  per-method message and `return` BEFORE the unsafe `&mut Self`
  materialisation. RAII drop guard removes on scope exit.
  - **Mechanism**: V8 TypeError, NOT `panic!`. Rust panic can't unwind
    through V8's C++ frames cleanly — empirically that surfaces as
    "fatal runtime error: failed to initiate panic, error 5" + SIGABRT
    on Linux. A V8 exception propagates the same way every other
    macro-emitted error already does (brand-check `Illegal invocation`,
    `[EnforceRange]` TypeError, etc.).
  - **Granularity trade-off**: per-method, per-instance. The set is
    instance-keyed (no false positives across distinct `Foo`
    instances), and there's a separate set per Rust method (no false
    positives across `Foo::a` calling `Foo::b` on the same instance).
    Real-world re-entry via JS callback overwhelmingly hits the SAME
    method (`this.method(...)` from a callback method registered),
    which is what the guard catches.
  - **Cost**: emitted ONLY for `&mut self` methods — `&self` callbacks
    skip the guard. Per-call overhead is one HashSet insert + one
    remove on the steady-state path; the set has 0 or 1 entries
    typically.
  - **Pre-fix symptom**: classes that wrapped state in an inner
    `RefCell` would panic with `RefCell already mutably borrowed`
    from deep inside V8 on re-entry; classes without an inner cell
    silently corrupted memory.
  - Lands: commit `7ce7f260` (codegen + 4 smoke tests in
    `tests/v8_reentrancy_smoke.rs`).

- **`[Clamp]` integer coercion** — `ClampU16` / `ClampU32` / `ClampI32`
  / `ClampU64` / `ClampI64` newtypes in `zeroship_runtime::clamp`
  implement WebIDL `[Clamp]` ConvertToInt: NaN → 0, < min → min, > max
  → max, otherwise round-half-even (banker's rounding) per
  https://webidl.spec.whatwg.org/#abstract-opdef-converttoint step 8.
  Unlike `[EnforceRange]` there is NO TypeError path — `[Clamp]` is
  the lenient counterpart. The 64-bit widths cap at `2^53 - 1` (JS
  Number precision boundary) on both sides; 32-bit widths cap at
  `i32::MIN..=i32::MAX` / `0..=u32::MAX`.
  - Macro detection by ident in `lib.rs::clamp_kind`; emission in
    `gen_extract` mirrors the `EnforceRangeU64` path but never throws.
  - Used (when migrated) by Streams chunk-size strategies (`[Clamp]
    unsigned long`), Blob.slice (`[Clamp] long long`), WebSocket close
    code (`[Clamp] unsigned short` — currently hand-rolled in
    `websocket_native::algorithms::clamp_unsigned_short`).
  - Lands: commit `dc26721d` (codegen + 5 smoke tests in
    `tests/v8_clamp_smoke.rs`).

- **`WebIdlConvertible` trait + `read_sequence<T>` / `read_record<K, V>`
  helpers** — WebIDL §3.13.16 (sequence) and §3.13.18 (record). The
  trait is the JS-value → Rust-type conversion at the WebIDL boundary;
  hand-implemented for primitives (USVString, ByteString, String, bool,
  u32, i32, f64, Option<T>, Local<Value>) and auto-implemented by the
  WebIdlDict / WebIdlEnum derives below. `read_sequence` iterates
  `@@iterator`; `read_record` iterates own enumerable property names
  in canonical (numeric ascending → string insertion-order) order.
  Non-iterable / non-object inputs throw TypeError.
  - Lives in `crates/runtime/src/webidl/convert.rs`. Re-exported as
    `zeroship_runtime::convert::*`.
  - Lands: commit `6806f9e5` (codegen + 14 smoke tests in
    `tests/v8_webidl_convert_smoke.rs`).

- **`#[derive(WebIdlDict)]` for dictionary parsing** — WebIDL §3.10.
  Generates `from_v8(scope, value) -> Result<Self, OpError>` from a
  struct with named fields. Each field type must implement
  `WebIdlConvertible`. Override the JS-side member name with
  `#[webidl_name = "..."]` (default = ident verbatim). `null` /
  `undefined` produce `Self::default()`; non-Object → TypeError;
  per-member errors propagate. Also emits `impl WebIdlConvertible for
  Self` so dicts compose inside sequence<T>, record<K, V>, and other
  dicts (the blanket Option<T: WebIdlConvertible> impl lifts
  `Option<Inner>` through naturally).
  - Reserved hooks (documented but not implemented in v1):
    `#[webidl_dict(enforce_range)]` for [EnforceRange] integer fields,
    `#[webidl_dict(custom_extractor = "fn_name")]` for non-Convertible
    field types. Trait-based dispatch covers every fetch / streams /
    WebSocket / WebCrypto dict member type today.
  - Migration follow-ups (one PR each, deferred to subagent):
    RequestInit, ResponseInit, BlobPropertyBag, EventInit,
    FilePropertyBag, QueuingStrategyInit,
    ReadableStreamGetReaderOptions. ~40 LOC each.
  - Lands: commit `ba7081b8` (codegen + 16 smoke tests in
    `tests/v8_webidl_dict_smoke.rs`).

- **`#[derive(WebIdlEnum)]` for enum types** — WebIDL §3.7.10. Generates
  `from_str` / `as_str` / `WebIdlConvertible` for unit-variant enums.
  Default name = ident kebab-cased (`NoCors` → `"no-cors"`); override
  per-variant with `#[webidl_name = "..."]`. The `from_v8` impl
  ToStrings the value (Symbols → TypeError naturally) then runs
  `from_str`; unknown name → TypeError per §3.13.7 step 4 with both
  the offending value AND the accepted-name set in the message.
  - The `pascal_to_kebab` helper preserves consecutive uppercase as a
    single lowercase run (`URL` → `"url"`, not `"u-r-l"`) so initialisms
    work without per-variant overrides. 4 inline unit tests.
  - Migration follow-ups (deferred): RequestMode, RequestCache,
    RequestRedirect, RequestCredentials, RequestDestination,
    ReferrerPolicy, ResponseType, ReadableStreamReaderMode,
    ReadableStreamType.
  - Lands: commit `40494fa3` (codegen + 14 smoke tests in
    `tests/v8_webidl_enum_smoke.rs`).

- **`#[v8_iterable(key = K, value = V)]` for default pair iterators** —
  WebIDL §3.7.10.2 (default iterators) + §3.7.10.3 (forEach). On a
  `#[v8_class]` impl block, emits the full pair-iterator surface
  (keys / values / entries / forEach / @@iterator) plus a companion
  `<Class>Iterator` class — from a single user-supplied
  `value_pairs(&self) -> Vec<(K, V)>` method.
  - **Iteration model**: snapshot. The factory clones `value_pairs()`
    once at factory-call time and the iterator walks the snapshot.
    Spec mandates LIVE iteration; this is a deliberate simplification
    documented in the codegen's doc-comment. Existing live-iteration
    consumers (Headers, URLSearchParams, FormData iterators) keep
    their hand-rolled implementations.
  - **Type bounds**: K ∈ {ByteString, USVString, String, u32};
    V ∈ same set + Vec<u8> (yielded as Uint8Array).
  - **Brand check**: every factory + forEach reuses the parent's
    `__brand_check_<Class>`; cross-class deception
    (`MyMap.prototype.keys.call(other)`) throws "Illegal invocation".
  - Iterator constructor is locked: `new <Class>Iterator()` throws.
  - Migration follow-ups (deferred — separate PR per class to keep
    risk small): URLSearchParamsIterator (~150 LOC), HeadersIterator
    (live; needs derive extension), FormDataIterator. Total saving
    estimated ~400 LOC across 3 classes.
  - Lands: commit `506a588f` (codegen + 14 smoke tests in
    `tests/v8_iterable_smoke.rs`).

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

### Lifetime-tied `Local<'s, T>` returns

The synthetic `&mut PinScope` reborrow shortens the returned `Local`'s
lifetime. URL-native added a workaround: skip the reborrow when the
param is named `scope`. Streams hit it in different methods. Generalise:
detect when return type is `Local<'s, _>` tied to a scope arg and skip
the reborrow systematically.

### Migrate hand-rolled `[SameObject]` getters to `#[v8_getter(same_object)]`

The macro now ships the attribute (commit `3cb0fe11`) but the existing
hand-rolled SameObject implementations (`Request.headers` in
`fetch_request.rs:299`, `Response.headers`, `URL.searchParams`) still
hand-roll the V8 Private symbol stash. Migrate each to the attribute
to delete the boilerplate and keep one cache implementation in the
codebase.

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

### Migration follow-ups (Tier 3 derives, one PR each)

The Tier 3 derives (`WebIdlDict`, `WebIdlEnum`, `v8_iterable`) are
shipped (commits `6806f9e5`, `ba7081b8`, `40494fa3`, `506a588f`) but
existing classes still hand-roll the equivalent code. Migrate one
class per PR to keep risk small:

**WebIdlDict migration candidates** (largest hand-rolled dicts):
  - `RequestInit` — `crates/runtime/src/web/fetch/request.rs` (~16
    members; saves ~40 LOC)
  - `ResponseInit` — `crates/runtime/src/web/fetch/response.rs`
  - `BlobPropertyBag` — `crates/runtime/src/web/blob/`
  - `EventInit` / `MessageEventInit` / `CloseEventInit` /
    `CustomEventInit` — `crates/runtime/src/web/dom/`
  - `QueuingStrategyInit`, `ReadableStreamGetReaderOptions` —
    `crates/runtime/src/web/streams/`
  - WebCrypto algorithm-init dicts — `crates/runtime/src/web/crypto/`

**WebIdlEnum migration candidates**:
  - `RequestMode`, `RequestCache`, `RequestRedirect`,
    `RequestCredentials`, `RequestDestination` — fetch
  - `ReferrerPolicy` — fetch
  - `ResponseType` — fetch
  - `ReadableStreamReaderMode`, `ReadableStreamType` — streams

**`#[v8_iterable]` migration candidates** (note: snapshot vs. live —
each requires assessing which mode the spec requires for the class):
  - `URLSearchParamsIterator` (~150 LOC) — `crates/runtime/src/web/url/search_params.rs`
  - `HeadersIterator` (~150 LOC) — `crates/runtime/src/web/headers.rs`.
    LIVE iteration required; deriving needs a "live" mode addition
    OR keep hand-rolled.
  - `FormDataIterator` (~100 LOC) — `crates/runtime/src/web/dom/form_data.rs`

Total estimated savings: ~400 LOC of iterator boilerplate, ~200 LOC
of dict-parsing boilerplate, ~100 LOC of enum-parsing boilerplate.
