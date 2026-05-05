# runtime-macros TODO

Closed items keep their commit hash so the rationale stays grep-able.
Open items track macro extensions consumers can't currently express.

Audit doc: `docs/reviews/v8-class-audit-2026-05-04.md` (Part B = macro
gaps; cross-referenced as `[B.N]` below).
Consumer-side migrations: `crates/runtime/TODO.md`.

---

## Done

### Core `#[v8_class]`

- **Brand check** via cached prototype walk (WebIDL §3.7) — `c95915e1`,
  lazy-capture follow-up `b0339e23`. Per-class `__brand_check_<Class>`
  walks ≤32 prototype links; subclasses (`#[v8_inherit]`) match.
- **`must_new`** constructor TypeError for `Foo()` without `new` — default-on,
  opt-out via `#[v8_constructor(callable_no_new)]` — `5b16b016`.
- **`[SameObject]` getter cache** — `#[v8_getter(same_object)]` stashes
  via per-class private symbol — `3cb0fe11`.
- **`[NewObject]` default** — confirmed: default is no-cache; `[SameObject]`
  is the opt-in. Test only — `4a557dc4`.
- **Re-entrancy guard** — `&mut self` callbacks throw V8 TypeError on
  reentrant invoke (per-method, per-instance HashSet) — `7ce7f260`.
- **`[Clamp]` newtypes** — `ClampU16/U32/I32/U64/I64` in
  `zeroship_runtime::clamp`. `[EnforceRange]` companion already shipped — `dc26721d`.
- **`Wrap{U8,U16,U32,I8,I16,I32}` newtypes** — default-case integer
  coercion (no `[Clamp]` / `[EnforceRange]`): NaN/Infinity → 0,
  truncate toward zero, modulo 2^N, signedness reinterpret. Lives in
  `zeroship_runtime::wrap`; macro detects via `wrap_kind` mirror of
  `clamp_kind`. Unblocks CloseEvent.code (`unsigned short` default).
  [MAC-17 / B.9] — `<commit>`.
- **Same-name getter+setter pairing** via `#[v8_name = "..."]` —
  paired install as one accessor descriptor. Codegen landed
  incrementally (`020a545`, `0dbb753`, `3806341`); smoke test only — `ce68f10`.
- **`#[v8_async_method]`** — async methods compile to a sync V8 callback
  + `PromiseResolver` + spawn via `state.spawned_ops`. `&mut self`
  rejected — `0a26d45b`, `17b2e880`, `caa4f450`.
- **`#[v8_static_method]` / `#[v8_static_getter]`** — WebIDL §3.7.4
  static operations / attributes installed on the constructor template
  (not the prototype). No receiver, no brand check, no internal-field
  deref; a `self` arg triggers a `compile_error!`. Unblocks 9 sites
  (Response.error/json/redirect, URL.canParse/parse, AbortSignal.abort/
  timeout/any, ReadableStream.from). [MAC-07 / B.1] — `<commit>`.
- **Public `__zs_is_<Class>`** — `pub fn __zs_is_<Class>(scope, v: Local<Value>)
  -> bool` emitted alongside `<Class>::install`. Re-exports the per-class
  brand check for cross-class type queries; eliminates fragile
  `instance_of(globalThis.X)` shape. [MAC-12 / B.11] — `<commit>`.

### WebIDL derives (Tier 3)

- **`WebIdlConvertible`** trait + `read_sequence<T>` / `read_record<K,V>`
  helpers (WebIDL §3.13.16/§3.13.18) — `6806f9e5`.
- **`#[derive(WebIdlDict)]`** for dictionary parsing (§3.10) —
  `ba7081b8`.
- **`#[derive(WebIdlEnum)]`** for enum types (§3.7.10), kebab-case
  default with `#[webidl_name = "..."]` override — `40494fa3`.
- **`#[v8_iterable]`** default pair iterators (§3.7.10.2/§3.7.10.3).
  Snapshot mode `506a588f`; `mode = live` `5901d68`. MAC-09 follow-up
  (`ad49b5a`) lets `value_pairs` accept `&mut self` and an optional
  `&mut PinScope` arg — the macro sniffs the receiver/arg shape and
  promotes recovery to `*mut Self` + `&mut *ptr` per call, with a
  per-instance re-entrancy guard. MAC-14 follow-up (`cc945cc`) adds
  `value_marshal = some_fn` for arbitrary V types (skipping the
  built-in classifier so unions / `Local<Value>` shapes work). Same
  commit fixes the `entries === [Symbol.iterator]` identity bug —
  both keys now share a single FunctionTemplate.
- **`#[v8_async_iterable(method = "name")]`** — alias
  `[Symbol.asyncIterator]` to a method that already exists per WebIDL
  §3.7.10.5. Emits a fresh FunctionTemplate wrapping the method's
  callback, sets `class_name(method)`, installs on the prototype
  template. [MAC-18 / B.12] — `<commit>`.
- **`#[v8_const(NAME = LIT)]`** — repeatable impl-block-level attribute
  declaring WebIDL §3.7.5 interface constants. Installs on BOTH the
  constructor template and the prototype template with `READ_ONLY |
  DONT_DELETE`; literal type suffix (`u16` / `u32` / `i32`) selects
  V8 materialiser. Unblocks DOMException's 25 legacy codes + Event's
  phase constants. [MAC-15 / B.6] — `<commit>`.

### WebIDL derives (Tier 4)

- **`#[webidl_enum(case_insensitive)]`** — ASCII case-insensitive
  matching. WebCrypto §15. `92f4632`.
- **`#[webidl_enum(silent_default)]`** — fall-through to `Self::default()`
  on unknown; `from_v8` never throws. Fetch `RedirectMode` /
  `CredentialsMode`, WebSocket `BinaryType`. `e644311`.
- **`#[webidl_dict_member(reject_null)]`** — null → TypeError; `undefined`
  still falls through to default. `AddEventListenerOptions.signal`. `0b8564d`.
- **`DictOrBool<T>`** wrapper for `(<dict> or boolean)` union members.
  `AddEventListenerOptions` whole. `5ceb593`.
- **`tc_scope` user-exception preservation** in `WebIdlDict` /
  `WebIdlEnum` extraction. Adds `OpErrorKind::JsValue(Global<v8::Value>)`;
  per-member extraction wraps `WebIdlConvertible::from_v8` in
  `v8::TryCatch` and rethrows the user's exception verbatim. `8065cc0`.

### Constructor hooks

- **MAC-02 `#[v8_constructor(post_init = "fn_name")]`** — post-construction
  hook that runs AFTER the box is installed in V8 internal-field 0
  and BEFORE the constructor returns to JS. Unblocks streams Reader /
  Writer / BYOBReader / TransformStream migrations to `#[v8_class]`,
  plus the SameObject `Request.headers` mint.
  - Hook signature: `fn(&mut PinScope, Local<Object>) -> Result<(), OpError>`.
    No `&Self` from the macro — user reads box state via
    `with_state(scope, this, |s| ...)` (the existing streams pattern),
    eliminating aliasing-vs-`&mut self`-method risk by construction
    (design §5.5 Option C).
  - Per the design: must_new + post_init = post_init never runs if
    must-new throws (early return); callable_no_new + post_init =
    post_init only fires for `is_construct_call() == true` (skip
    private-symbol writes on globalThis); `#[v8_inherit]` = derived's
    hook runs, base's does NOT auto-chain (matches V8's
    constructor semantics; explicit chaining works — §5.4 worked
    AbortSignal example).
  - Err path: throws via the existing OpError → Exception arm
    (TypeError / RangeError / Error / DOMException / NodeError).
    The half-constructed Box stays installed in field 0 until V8's
    weak finalizer reclaims the wrapper on the next GC sweep
    (lazy-drop is the v1 contract per §4.4; eager-drop deferred to
    MAC-NN if production failure rates warrant).
  - **Phase 1 only.** No consumer migrations land in this commit;
    Reader / BYOBReader / Writer / TransformStream migrations are
    follow-up PRs (design §7.1).
  - Compile-fail diagnostics for the four malformed shapes
    (missing fn, wrong signature, non-string value, non-ident string)
    are locked in via `trybuild` in
    `crates/runtime/tests/compile_fail_post_init/`.
  - Design: `docs/proposals/macro-constructor-post-init.md`.
  - Lands: commit `edecd62` (codegen + 9 smoke tests in
    `tests/v8_post_init_smoke.rs` + 4 trybuild compile-fail
    snapshots).

- **V8 fastcall annotation** — `#[v8_method(fastcall)]` /
  `#[v8_getter(fastcall)]` emits a CFunction shim alongside the
  slow-path FunctionCallback. V8 TurboFan inlines the typed-shape
  call at hot sites, skipping the full prologue (~30-100ns/call).
  - Wiring via `FunctionTemplate::builder(slow).build_fast(scope, &[FAST])`.
  - Box<Self> stored as External in slot 0 (existing) AND as aligned
    pointer in slot 1; fastcall shim reads via
    `get_aligned_pointer_from_internal_field(1, 0)` — single load,
    no scope.
  - Brand check on fast path: trust V8's CFunction-typed receiver
    (TurboFan inserts hidden-class shape check before dispatch;
    cross-class deception deopts to the slow callback's prototype-walk
    brand check). One load saved per fast call.
  - Compile-time validation: rejects `&mut self`, `String`/`Vec<u8>`/
    `Option<T>` returns, unsupported arg types. Compile-fail doctests
    in `runtime/src/lib.rs` document the rules.
  - Allowed shapes: `&self` + (`bool`/`i32`/`u32`/`i64`/`u64`/`f32`/
    `f64`/`ByteString`)* args + (`bool`/`i32`/`u32`/`i64`/`u64`/`f32`/
    `f64`/`()`) or `Result<primitive, OpError>` return.
  - `Result<Err>` arms throw via `callback_scope!(unsafe ...)` +
    `type_error` and return a sentinel; V8 ignores when an exception
    is pending and re-routes to the slow path.
  - Lands: commit `1e4e0b6` (macro feature + 9 smoke tests in
    `tests/v8_fastcall_smoke.rs`); follow-up `9721a76` migrates
    `Headers.has` (Tier 1 candidate). Bench-validation results in
    `crates/runtime/benches/results-2026-05-04-after-fastcall.txt`.
  - **Migration status of Tier 1 ROI candidates** (from the original
    spec):
    - `Headers.has(name)` — **MIGRATED** (commit `9721a76`).
    - `Headers.get(name)` — **DEFERRED**: return type
      `Result<Option<Vec<u8>>, OpError>` doesn't fit fast API
      (Option = no null sentinel; Vec<u8> allocates; v147 rusty_v8
      lacks SeqOneByteString writer/out-param). Tracked as a
      "FastByteStringWriter adapter" follow-up.
    - `AbortSignal.aborted` — Open. Pure `(this) → bool`, easy.
    - `URL.protocol` / `.host` / `.pathname` — Open. Need
      USVString writer (same blocker as Headers.get).
    - `crypto.getRandomValues(buf)` — Open. Need
      FastApiTypedArray<u8>; not yet wired in macro.
    - `Streams.desiredSize` — Open. `(this) → f64`, easy.
    - `URLSearchParams.size` — Open. `(this) → u32`, easy.
  - **Bench validation outcome (2026-05-04)**: httpGet 16w =
    323k req/s avg (3-run) vs 316k baseline. +2.3%, within noise.
    scenarios.js does `headers.get("upgrade")`, NOT `.has`, so the
    migrated Headers.has doesn't sit on the bench hot path. The
    +2.3% likely comes from FunctionTemplate-construction shape
    change being marginally warmer in the inline cache, not actual
    fast-path firing on `/hello`. To realise the regression doc's
    50-80k estimate, follow-ups must migrate `Headers.get` (needs
    string-writer adapter) or `Response.json`.

## Open

### High — blocks major class migrations

- **MAC-01 `#[v8_state(Inner)]`** — internal-field-0 type ≠ `Box<Self>`.
  Blocks Request (`web/fetch/request.rs`, ~700 LOC) and Response
  (~550 LOC) whole-class migration. Most invasive change in this list.
  [B.13]

### Medium — multi-consumer or significant LOC saved

- **MAC-08 `#[v8_getter(same_object, project = field)]`** — cache on
  state field instead of private symbol (Request.clone semantics). 5
  consumers: URL.searchParams, Request.headers/.signal, Response.headers,
  AbortController.signal. [B.2]
- **MAC-10 `#[v8_class(install_on_prototype_template)]`** — for spec
  base classes inherited via `#[v8_inherit]`. Blocks EventTarget. [B.8]
- **MAC-11 `#[v8_method(returns_promise)]`** — sync-but-Promise methods.
  Blob/File `text/arrayBuffer/bytes` (6 sites). -50 LOC. [B.10]
### Low — single-consumer or polish

- **MAC-16 `#[webidl_required]`** dict-member flag — TypeError on
  `undefined` for required members. QueuingStrategyInit + future. [B.7]
- **Lifetime-tied `Local<'s, T>` returns** — generalize URL-native's
  `param-named-scope` workaround to detect `Local<'s, _>` tied to a
  scope arg.
- **Generic return type detection** — trait-based dispatch (currently a
  fixed list `bool/u32/i32/f64/String`). Unblocks `u64` byte-counter
  getters, etc.
- **`#[reject_shared]` on setter args** — currently methods only.
  CompressionStream uses methods, so not blocking.
- **Better compile errors** — friendly diagnostics for common shapes
  (~50 LOC). Today most user mistakes surface as cryptic syn errors.

---

## Consumer migration pointers

Tracked in `crates/runtime/TODO.md` "V8 class macro migration follow-ups"
— not duplicated here. Highlights:

- **Whole-class migrations** (blocked on MAC-01 / MAC-02): Request,
  Response, ReadableStream, WritableStream, TransformStream,
  Reader/Writer/BYOBReader.
- **Iterator migrations** — MIGRATED via MAC-09 (+ MAC-14 for
  FormData):
    - HeadersIterator → `#[v8_iterable(mode = live)]` `5677051`
      (-333 LOC)
    - URLSearchParamsIterator → `#[v8_iterable(mode = live)]`
      `da77c03` (-314 LOC)
    - FormDataIterator → `#[v8_iterable(mode = live, value_marshal
      = entry_value_to_v8)]` `cc945cc` (-214 LOC)
- **Hand-rolled `[SameObject]` migrations** (blocked on MAC-08):
  URL.searchParams, Request/Response.headers, AbortController.signal.
- **Static methods migrations** (blocked on MAC-07): 9 sites.
