# v8_class Audit — 2026-05-04 @ ca71a55

Read-only audit of `crates/runtime/src/web/` for `#[v8_class]` consumer
adoption gaps and macro-side capability gaps revealed by hand-rolled
patterns. The macro feature set is the "Done" section of
`crates/runtime-macros/TODO.md`. Already-in-flight Tier 4 gaps
(`WebIdlEnum(case_insensitive)`, `silent_default`, dict-member
`reject_null`, `DictOrBool<T>`, dict/enum `tc_scope` exception
preservation) are NOT re-listed.

26 files in `crates/runtime/src/web/` use `#[v8_class]`. The biggest
hold-outs are Request, Response, all of `streams/` (8 files), and the
listener-bearing parents (EventTarget). Each is hand-rolled for a
specific macro gap that this audit catalogs.

---

## Part A — Consumer migration opportunities

Findings ordered within each tier by leverage (LOC × spec-correctness).

### A.1 Highest leverage

#### A.1.1 Request — full `#[v8_class]` migration (blocked partially; see Part B)
- Consumer: `crates/runtime/src/web/fetch/request.rs:182-1341` (~1160 LOC of class wiring)
- Pattern: hand-rolled FunctionTemplate, hand-rolled `install_*_getter`
  per scalar, `string_getter!` / `bool_getter!` macros, hand-rolled
  brand check (`brand_check`), hand-rolled finalizer, hand-rolled
  `[SameObject]` for `headers` / `signal` (via stored `Global<Object>`
  on `RequestState`), hand-rolled `must_new` (constructor doesn't
  check, but the `body` extraction throws if `this` isn't right).
- Macro features: `#[v8_class]` + `#[v8_constructor]` (must_new) +
  `#[v8_getter]` for the 14 scalar getters + `#[v8_getter(same_object)]`
  for `headers` / `signal` (when Part B `Global<Object>` projection
  ships).
- LOC delta: -700 LOC (the 14 getters compress to ~100 LOC, the
  constructor's WebIDL boilerplate stays but the wiring goes).
- Spec: https://fetch.spec.whatwg.org/#request
- Risk: file comment (`fetch_request.rs:185-198`) lists 3 reasons the
  macro can't be used today: (1) `Box<RequestState>` shape vs the
  macro's `Box<Self>` requirement; (2) `[SameObject]` getters that
  return a stored `Global<Object>` (the macro's `same_object`
  attribute mints fresh Objects, not project from state); (3) Body
  trait dispatch via `install_body_methods::<Request>(proto)` doesn't
  fit the macro's per-method install path. Item 2 is a NEW Part B
  gap; item 1 needs `#[v8_state(Inner)]` to point at a separate
  state struct; item 3 needs an "external prototype installer" hook.

#### A.1.2 Response — full `#[v8_class]` migration (same pattern as Request)
- Consumer: `crates/runtime/src/web/fetch/response.rs:222-1131` (~900 LOC class wiring)
- Pattern: identical to Request — same hand-rolled FunctionTemplate,
  same `install_*_getter` per scalar (8 getters), same `[SameObject]`
  hand-roll for `headers`, plus 3 hand-rolled static methods
  (`Response.error`, `.redirect`, `.json` — see A.1.7).
- Macro features: same as Request, plus `#[v8_static_method]` (Part B).
- LOC delta: -550 LOC.
- Spec: https://fetch.spec.whatwg.org/#response
- Risk: same 3 blockers as Request (state pointer indirection,
  SameObject Global projection, Body trait dispatch). Static methods
  are an additional Part B gap.

#### A.1.3 ReadableStream — `#[v8_class]` migration
- Consumer: `crates/runtime/src/web/streams/readable.rs:212-1447` (~1200 LOC class wiring)
- Pattern: hand-rolled FunctionTemplate (line 222, `stream_class_template`),
  hand-rolled `is_construct_call` check (line 302), hand-rolled
  finalizer, hand-rolled `Symbol.toStringTag` install, hand-rolled
  `[Symbol.asyncIterator]` install (line 261).
- Macro features: `#[v8_class]` + `#[v8_constructor(must_new)]` +
  `#[v8_getter]` (locked) + `#[v8_method]` × 7 (cancel, getReader,
  pipeTo, pipeThrough, tee, values, plus static `from`).
- LOC delta: -300 LOC.
- Spec: https://streams.spec.whatwg.org/#rs-class
- Risk: constructor body has WebIDL `(underlyingSource, strategy)`
  union dispatch + spec-mandated re-parse for byte-stream HWM
  default — those stay, only the wiring goes. Two NEW Part B gaps:
  `Symbol.asyncIterator` install + the `from()` static method
  (line 1446 — `stream_class_fn.set(scope, from_key.into(), …)`).

#### A.1.4 WritableStream / TransformStream — `#[v8_class]` migration
- Consumers:
  - `crates/runtime/src/web/streams/writable.rs:303-605` (~300 LOC class wiring)
  - `crates/runtime/src/web/streams/transform.rs:280-905` (~500 LOC class wiring)
- Pattern: same as ReadableStream — hand-rolled class template,
  is_construct_call (lines 312 / 298), brand-symbol set_private
  (writable.rs:373-375, transform.rs:358-360).
- Macro feature: `#[v8_class]` + `#[v8_constructor(must_new)]`. The
  WebIDL union parsing in the constructor stays.
- LOC delta: -200 LOC combined.
- Spec: streams §4.2 / §5.2.
- Risk: writable/transform stash a `[[brand]]` private symbol on the
  wrapper (writable.rs:373-375). The macro's brand check (commit
  c95915e1) walks the prototype chain instead — same correctness, no
  symbol needed. Dropping the symbol changes wire-level behaviour
  observable through `obj[Symbol(__zs__brand__)]` pseudo-API, but
  no consumer reads it.

#### A.1.5 ReadableStreamDefaultReader / BYOBReader / WritableStreamDefaultWriter
- Consumers:
  - `crates/runtime/src/web/streams/readable_default_reader.rs:188-742` (~400 LOC class wiring)
  - `crates/runtime/src/web/streams/readable_byob_reader.rs:285-894` (~500 LOC)
  - `crates/runtime/src/web/streams/writable_writer.rs:166-795` (~600 LOC)
- Pattern: hand-rolled class template, hand-rolled `is_construct_call`,
  closed-promise resolver allocation (these need the resolver minted
  in the constructor itself — outside the macro's expressive range
  today), brand-symbol set_private (writable_writer.rs:268-270).
- Macro feature: `#[v8_class]` + `#[v8_constructor(must_new)]` + the
  brand check covers the symbol. Async methods (`read`, `cancel`,
  `releaseLock`, `closed`-promise) stay hand-rolled because they
  resolve from external pump events, not from `state.spawned_ops`.
- LOC delta: -250 LOC combined.
- Spec: streams §3.4 (DefaultReader), §3.5 (BYOB), §4.4 (Writer).
- Risk: NEW Part B gap — the constructor needs to allocate +
  set_private a Promise resolver (the closed-promise pair). The macro
  has no hook for that. Either: (a) add `#[v8_constructor(post_init)]`
  callback that runs after the macro builds the wrapper but before
  the user method, OR (b) pass `args.this()` to the user constructor.
  Option (b) is the broader fix and re-enables the rest of streams.

#### A.1.6 Migrate hand-rolled `[SameObject]` getters (already in macro TODO.md "Open")
- Consumers:
  - `crates/runtime/src/web/url/url.rs:574-651` (`url.searchParams`,
    ~80 LOC manual private-symbol cache + JS-instance allocation)
  - `crates/runtime/src/web/fetch/request.rs:1167-1183` (`request.headers`)
  - `crates/runtime/src/web/fetch/request.rs:1185-1210` (`request.signal`,
    plus lazy-mint logic that's complementary to the cache)
  - `crates/runtime/src/web/fetch/response.rs` (`response.headers`)
  - `crates/runtime/src/web/dom/abort_controller.rs:9-65`
    (`controller.signal` — comment claims SameObject; check actual
    getter shape)
- Pattern: each manually mints + stashes a `Global<v8::Object>` on the
  state struct, returns a Local on the cached path. Works today; the
  macro's `#[v8_getter(same_object)]` doesn't yet support projecting a
  pre-existing `Global` (it mints the Object on first call from the
  user method's return). See B.2 — closing this gap unblocks the
  migration.
- Macro feature: `#[v8_getter(same_object)]` (existing) + Part B.2
  (project from state, not mint).
- LOC delta: -200 LOC across 5 sites.
- Spec: WebIDL §3.7.5 `[SameObject]`; Fetch §5.4 (Request.headers,
  Request.signal); Fetch §5.5 (Response.headers); URL §4.5
  (URL.searchParams).
- Risk: the cached Global is owned by the wrapper's state for
  semantic reasons (the SP wrapper's parent_url back-ref needs to
  outlive the SP itself; Headers identity must survive Request.clone).
  A SameObject macro that ALWAYS uses a private symbol (instead of
  state) would break the Request.clone() path that copies the headers
  Global into a new Request. Macro feature must support BOTH
  variants.

#### A.1.7 Static methods — many hand-rolled `install_static`
- Consumers:
  - `crates/runtime/src/web/dom/abort_signal.rs:742-744`
    (`AbortSignal.abort` / `.timeout` / `.any`)
  - `crates/runtime/src/web/fetch/response.rs:264-266`
    (`Response.error` / `.redirect` / `.json`)
  - `crates/runtime/src/web/url/url.rs:399-412` (`URL.canParse` / `.parse`)
  - `crates/runtime/src/web/streams/readable.rs:1446`
    (`ReadableStream.from`)
  - `crates/runtime/src/web/dom/exception.rs:303-308` (legacy
    constants — see A.2.4)
- Pattern: hand-rolled `install_static(scope, class_fn, "name", cb)`
  helper, raw FunctionCallback per static. Each ~30-80 LOC.
- Macro feature: NEW — `#[v8_static_method]` (see B.1).
- LOC delta: -200 LOC across 9 statics.
- Spec: each static has its own §; not gated on a macro feature today.
- Risk: macro must distinguish static (no receiver) from method (has
  receiver) and skip the brand check for statics. Trivial.

### A.2 Medium leverage

#### A.2.1 Headers — migrate to `#[v8_iterable(mode = live)]`
- Consumer: `crates/runtime/src/web/headers.rs:705-1203` (~500 LOC of
  iterator + factory + forEach hand-roll)
- Pattern: explicit `HeadersIterator` `#[v8_class]` + hand-rolled
  factories + hand-rolled forEach (lines 1044-1100). Already on
  the macro TODO "Open" list as a known migration; called out
  separately here because the migration is BLOCKED until B.4
  (`&mut self` value_pairs) ships — Headers' lazy-sort cache
  requires `&mut self`.
- Macro feature: `#[v8_iterable(key = ByteString, value = Vec<u8>, mode = live)]`
  + Part B.4 (`value_pairs(&mut self)`).
- LOC delta: -350 LOC after B.4 lands.
- Spec: WebIDL §3.7.10.2 (live iterators), Fetch §2.2.1.
- Risk: see B.4. Snapshot mode would work today by paying the
  sort-and-combine cost on every factory invocation, but is spec-
  noncompliant for live mutation.

#### A.2.2 URLSearchParamsIterator — migrate to `#[v8_iterable(mode = live)]`
- Consumer: `crates/runtime/src/web/url/search_params.rs:705-1012` (~150 LOC)
- Pattern: hand-rolled `URLSearchParamsIterator` class + factory +
  forEach. Local brand check `is_url_search_params` (lines 173-198)
  is now redundant with the macro's brand check (commit c95915e1)
  but still hand-coded.
- Macro feature: `#[v8_iterable(key = USVString, value = USVString, mode = live)]`
  + Part B.4 (`&mut self` for `sync_from_parent`).
- LOC delta: -150 LOC after B.4.
- Spec: WebIDL §3.7.10.2, URL §6.
- Risk: SP's iterator has the bound-mode parent-URL back-reference;
  the macro's iterator state holds only `Global<Object>` of the
  parent. The `value_pairs(&mut self)` body would need to call
  `self.sync_from_parent(scope)` first — macro must pass `scope`.

#### A.2.3 FormDataIterator — migrate to `#[v8_iterable(mode = live)]`
- Consumer: `crates/runtime/src/web/dom/form_data.rs:160-256` (~100 LOC)
- Pattern: hand-rolled iterator. FormData entries are `Vec<(String, FormDataValue)>`
  where FormDataValue is `String | File-Global<Object>` — the
  iterator yields strings or live File JS objects.
- Macro feature: `#[v8_iterable(key = USVString, value = ???)]` —
  blocked because the macro's value type set is
  `{ByteString, USVString, String, u32, Vec<u8>}`. FormDataValue is
  a union (USVString or `Global<Object>`). NEW Part B gap (B.5).
- LOC delta: -90 LOC if B.5 ships.
- Spec: WebIDL §3.7.10.2, XHR §5.
- Risk: a `Global<Object>` value in the iterator state requires the
  macro to handle non-cloneable types. Today only by-value types are
  supported. See B.5.

#### A.2.4 DOMException — migrate legacy code constants to `#[v8_const]`
- Consumer: `crates/runtime/src/web/dom/exception.rs:84-110, 303-308`
  (25 constants installed via two-loop boilerplate)
- Pattern: `LEGACY_CODE_CONSTANTS: &[(&str, u16)]` table + a hand-
  rolled install loop that sets each on BOTH the constructor function
  AND the prototype.
- Macro feature: NEW — `#[v8_const(NAME = u16)]` impl-block-level
  attribute that the install codegen reads and emits the
  set-on-both-targets boilerplate. See B.6.
- LOC delta: -50 LOC (the constants list shrinks to 25 individual
  attribute lines, the install loop disappears).
- Spec: WebIDL §3.7.5.
- Risk: trivial; consumers are stable, and a `&[(&'static str, u16)]`
  table is a one-shot codegen target.

#### A.2.5 WebSocket close code — migrate to `ClampU16` newtype
- Consumer: `crates/runtime/src/web/websocket/algorithms.rs:34-92`
  (`clamp_unsigned_short` ~60 LOC) + WebSocket.close usage
- Pattern: hand-rolled IDL `[Clamp] unsigned short` per WebIDL
  §3.2.3-converttoint Clamp branch. Already called out in
  TODO.md "Done" section under `[Clamp]`.
- Macro feature: `ClampU16` newtype (already shipped commit dc26721d).
- LOC delta: -60 LOC.
- Spec: WHATWG WebSockets §3.1.
- Risk: hand-roll has a re-clamp safeguard for the 65535.5 → 65536
  edge case. The macro's `read_clamp_u16` does the same — verified
  in `crates/runtime/src/webidl/clamp.rs`.

#### A.2.6 Blob.slice — migrate to `ClampI64`
- Consumer: `crates/runtime/src/web/blob/blob.rs:121-160`
  (`clamp_long_long` + `_public` re-export, ~40 LOC) +
  `crates/runtime/src/web/blob/file.rs` (re-uses `clamp_long_long_public`).
- Pattern: hand-rolled IDL `[Clamp] long long` for `Blob.slice(start,
  end)`. Documented in macro TODO.md "Done" section under `[Clamp]`
  (`Blob.slice` listed).
- Macro feature: `ClampI64` newtype (already shipped).
- LOC delta: -40 LOC.
- Spec: File API §3.3.6 + WebIDL §3.2.4.
- Risk: hand-roll uses `f64::round_ties_even`; macro uses identical
  logic per the ClampU16 verification.

#### A.2.7 streams/strategies.rs — `QueuingStrategyInit` is dict-parsed but
required-member check is hand-rolled
- Consumer: `crates/runtime/src/web/streams/strategies.rs:228-267`
- Pattern: `QueuingStrategyInit` IS a `WebIdlDict` already, but
  `highWaterMark` is required per IDL and the derive can't enforce
  that. The wrapper makes it `Option<f64>` and checks `None` at the
  call site.
- Macro feature: NEW — `#[webidl_required]` field-level attribute
  on WebIdlDict (see B.7).
- LOC delta: -10 LOC (delete the Option<>+None-check; field becomes
  bare `f64` with required semantics).
- Spec: WebIDL §3.10 step 5 + §3.2.20 dictionary required members.
- Risk: simple semantic; emits TypeError on missing-member with
  per-class message. Easy to add.

#### A.2.8 EventTarget brand check is now redundant
- Consumer: `crates/runtime/src/web/dom/event_target.rs:117-180`
- Pattern: EventTarget hand-rolls `__InstallSlot_EventTarget` to be
  compatible with the macro convention so `#[v8_inherit(EventTarget)]`
  resolves the SAME template. The reason for hand-rolling is the
  prototype_template-vs-prototype-object distinction (line 99-115)
  needed for FunctionTemplate inheritance. The brand check on
  `addEventListener` etc. could now lean on the macro's prototype-
  walk brand check instead of the hand-rolled "internal field 0 is
  External" check, but the dispatch path is hand-rolled.
- Macro feature: `#[v8_class]` + an extension to install methods on
  prototype_template (NEW B.8) instead of resolved prototype object.
- LOC delta: -50 LOC (the hand-rolled brand checks in
  `add_event_listener_callback` / `dispatch_event_callback` go).
- Spec: DOM §2.7.
- Risk: prototype_template installs vs. resolved-prototype installs is
  a semantic difference V8 enforces; getting the macro to support both
  is the ask. Already documented as the reason for hand-roll.

### A.3 Lower leverage / scattered

#### A.3.1 Five remaining `is_construct_call` checks
- Consumers: see Part A.1.3-A.1.5 — all 5 remaining
  `is_construct_call` calls (`grep -rn is_construct_call`) are inside
  classes that are migration candidates already enumerated. Each
  collapses to `#[v8_constructor(must_new)]` after the migration.

#### A.3.2 CloseEvent.code "default unsigned short modulo" — NEW newtype gap
- Consumer: `crates/runtime/src/web/dom/close_event.rs:62-88`
  (`convert_unsigned_short_modulo` ~30 LOC)
- Pattern: hand-rolled WebIDL default-case `unsigned short` (NaN→0,
  ±∞→0, truncate-toward-zero, modulo 2^16). NOT `[Clamp]` — that's
  WebSocket.close's `code` arg only. CloseEventInit.code uses the
  unattributed default.
- Macro feature: NEW — a `WrapU16` (default-case) newtype companion
  to the existing `ClampU16` / `EnforceRangeU64`. See B.9.
- LOC delta: -30 LOC.
- Spec: WebIDL §3.2.10 ConvertToInt default case + WHATWG
  WebSockets §3.2 (CloseEventInit).
- Risk: hand-roll uses `as i64 → as u32 & 0xFFFF`. Macro newtype
  would do the same; needs a unit test for the negative-modulo
  branch (-1 → 0xFFFF).

#### A.3.3 Blob/File `text()` / `arrayBuffer()` / `bytes()` — sync-but-Promise
- Consumers:
  - `crates/runtime/src/web/blob/blob.rs:629-678` (3 methods × ~15 LOC)
  - `crates/runtime/src/web/blob/file.rs:292-338` (3 methods, identical)
- Pattern: each manually allocates `PromiseResolver::new`, gets the
  promise, resolves immediately with the synchronously-computed value,
  returns the promise as `Local<Value>`. The user method returns
  `Local<Value>` (boxed promise) instead of the raw value because the
  spec mandates a Promise return.
- Macro feature: NEW — `#[v8_method(returns_promise)]` (or a new
  attribute name) that wraps a sync user-fn return value in a
  resolved Promise. Different from `#[v8_async_method]` (which spawns
  via `state.spawned_ops`); for these methods the value is already
  computed. See B.10.
- LOC delta: -50 LOC across 6 method bodies. More importantly, removes
  the 6 hand-rolled `unwrap()` on PromiseResolver.
- Spec: File API §3.3.7-9.
- Risk: macro must distinguish "Promise<T>" return type at parse time.
  The simplest spelling: `#[v8_method(returns_promise)] fn text(&self) -> Result<String, OpError>`,
  macro emits the resolver+resolve dance.

#### A.3.4 fetch_request.rs — RequestInit dict members
- Consumer: `crates/runtime/src/web/fetch/request.rs:430-477`
  (`copy_string_init` × 9 + `keepalive` bool)
- Pattern: hand-rolled `copy_string_init` helper (per-member loop) +
  ad-hoc dispatch for the boolean. Already on macro TODO.md "Open"
  for a `RequestInit` dict migration. Saves ~40 LOC. Blocked on
  Request's whole-class migration (A.1.1) AND on B.7
  (required-member checks for `headers`/`method`/`body` are not
  required, so this is an easier win than A.1.1).
- Macro feature: `#[derive(WebIdlDict)]` (existing).
- LOC delta: -40 LOC (already in TODO).
- Spec: Fetch §5.4 RequestInit.
- Risk: low — the existing helper does ToString lossy, the macro's
  String field does the same.

#### A.3.5 Hand-rolled `is_blob_instance` / `is_request_instance` /
`is_readable_stream_global_instance` brand checks
- Consumers:
  - `crates/runtime/src/web/blob/blob.rs:382-397` (`is_blob_instance`)
  - `crates/runtime/src/web/fetch/request.rs:842-854` (`is_request_instance`)
  - `crates/runtime/src/web/fetch/request.rs:859-875` (`is_readable_stream_global_instance`)
  - `crates/runtime/src/web/url/search_params.rs:173-198` (`is_url_search_params`)
- Pattern: per-class `obj.instance_of(scope, globalThis.Foo)` walks.
  These are CROSS-class brand checks (e.g. asking "is this arg a
  Blob?") — different from the receiver brand check the macro emits
  for own methods. The macro provides the latter; the former needs
  a per-class predicate exposed by the macro.
- Macro feature: NEW — emit a public `is_<class>(scope, value) -> bool`
  helper alongside `<Class>::install`. Lives at module scope so cross-
  class consumers can call it without re-implementing the prototype-
  chain walk. See B.11.
- LOC delta: -60 LOC across 4 sites; many more sites once classes
  migrate (Request.body extraction checks Blob-instance, FormData
  build checks File-instance, etc.).
- Spec: WebIDL §3.7 (each class has an associated brand).
- Risk: emit only when no user-defined `is_<class>` collides. The
  macro can name it `__zs_is_<class>` to avoid collision and re-export
  publicly via a documented helper.

---

## Part B — Macro gaps to fill

NEW gaps revealed by the audit, beyond the 5 already in flight (Tier 4).

### B.1 `#[v8_static_method]` and `#[v8_static_getter]`
- Consumers blocked: 9 statics across 4 classes (A.1.7).
- Pattern hand-rolled: `install_static(scope, class_fn, "name", cb)`
  installs a function on the constructor function itself (not the
  prototype). The macro today has no syntax for this.
- Proposed macro feature: a `#[v8_static_method]` / `#[v8_static_getter]`
  marker attribute consumed by `#[v8_class]`. Codegen: the install
  fn writes the function onto `class_fn` directly via `class_fn.set(...)`,
  no internal-field deref, no brand check.
- LOC delta if shipped: unblocks ~9 consumers, -200 LOC (Part A.1.7).
- Spec: most WebIDL static methods (§3.7.4 `static` operations).
- Risk: trivial. Static methods don't have a receiver, so the brand
  check / re-entrancy guard / `&mut self` plumbing is skipped.

### B.2 `#[v8_getter(same_object, project = field)]` — project from state
- Consumers blocked: `URL.searchParams`, `Request.headers`,
  `Request.signal`, `Response.headers`, `AbortController.signal`
  (5 consumers, A.1.6).
- Pattern hand-rolled: each stores a `RefCell<Option<v8::Global<v8::Object>>>`
  on the boxed state. The getter mints lazily on first call OR reads
  the cached Global. The macro's existing `same_object` attribute
  always uses a private symbol on the wrapper instance; consumers
  here use a struct field because the cached Global has cross-class
  semantics (Request.clone() wants to copy the Global into a new
  Request without a private-symbol read).
- Proposed macro feature: `#[v8_getter(same_object, project = field_name)]`
  variant where the user method takes `&self` and returns an
  `Option<v8::Global<v8::Object>>` projected from the named field.
  Codegen: if Some, return Local; if None, fall through to user
  method to mint and stash via the existing private-symbol path.
- LOC delta if shipped: unblocks 5 consumers, -200 LOC.
- Spec: WebIDL §3.7.5 `[SameObject]`. Spec doesn't mandate the
  storage location; private symbol vs state field is impl detail.
- Risk: needs a hook for "set this in state from an external code
  path" so Request.clone() can swap the headers Global. Or: keep the
  hand-roll for Request.clone() and use the macro for the
  read-only path. Audit-time research needed.

### B.3 `#[v8_constructor]` with access to `args.this()` / post-init hook
- Consumers blocked: Reader/Writer/BYOBReader (A.1.5), Request
  (A.1.1, partly), TransformStream (A.1.4) — all need to allocate a
  Promise resolver pair OR set a private symbol on the wrapper
  AS PART OF the constructor body.
- Pattern hand-rolled: each constructor body does
  `let resolver = v8::PromiseResolver::new(scope).unwrap();
   ... store resolver_global on state ... reader.set_private(...)`
  before the box is even allocated.
- Proposed macro feature: pass `&mut PinScope` AND `args` (or just
  `args.this()`) to the user constructor. Today the user constructor
  receives `scope: &mut PinScope` (already plumbed for arg
  extraction) but NOT `this`. Adding `this: v8::Local<v8::Object>` as
  a special-named param would let constructors stash private symbols.
  Alternative: `#[v8_constructor(post_init)]` named-fn hook that
  runs AFTER box install with access to `(scope, this, &Self)`.
- LOC delta if shipped: unblocks 4-5 consumers, -100 LOC of
  resolver/private-symbol install boilerplate.
- Spec: streams §3.4 (Default reader stores [[closedPromise]] resolver
  in constructor); writable §4.4 (Writer same).
- Risk: post_init hook is the cleaner shape — keeps the macro's box
  ownership story intact. `this`-access in the constructor body is
  more flexible but couples user code tighter to V8.

### B.4 `#[v8_iterable]` accept `value_pairs(&mut self)` (or
`value_pairs(&self, &mut PinScope)`)
- Consumers blocked: Headers (A.2.1 — needs `&mut self` for sort
  cache), URLSearchParams (A.2.2 — needs `&mut self` for
  sync_from_parent which itself needs `scope`), FormData (A.2.3 —
  fine with `&self`).
- Pattern hand-rolled: Headers' `value_pairs_to_iterate_over`
  (`headers.rs:319-324`) takes `&mut self` to populate the lazy
  sort-and-combine cache. URLSearchParams' equivalent walks parent
  URL via `sync_from_parent(&mut self, scope: &mut PinScope)`. The
  macro's spec at line 4 of `v8_iterable.rs` says
  `value_pairs(&self) -> Vec<(K, V)>` — too restrictive.
- Proposed macro feature: relax the receiver to `&mut self`. The
  macro's brand check + re-entrancy guard already cover the safety.
  Additionally accept an optional `&mut PinScope` arg for sync-from-
  parent type patterns. With both: a snapshot mode call goes:
  `let pairs = unsafe { &mut *raw }.value_pairs(scope);` (the scope
  is already in scope for the brand check — pass through).
- LOC delta if shipped: unblocks 3 large iterators (-400 LOC).
- Spec: iteration is observably stateful (lazy cache fills, parent
  URL syncs); spec doesn't dictate sync-vs-mut.
- Risk: re-entrancy: a value_pairs body that re-enters JS via the
  scope arg could trigger the macro's own re-entrancy guard. The
  macro guard is per-method; iteration's value_pairs is a unique
  method, so it gets its own slot.

### B.5 `#[v8_iterable]` accept `Global<Object>` value type
- Consumers blocked: FormData (A.2.3) — `FormDataValue::File(Global<Object>)`.
- Pattern hand-rolled: the iterator's value-marshaling is hand-coded
  to materialize `entry_value_to_v8(scope, &entry.1)` per yield.
- Proposed macro feature: extend `classify_ty` in
  `crates/runtime-macros/src/v8_iterable.rs:175-194` to recognise a
  `value = Local<Value>` or a custom-trait-based marshal. Live mode
  already needs to call into the parent each yield; adding
  Global-projection is a small step from there.
- LOC delta if shipped: unblocks FormDataIterator (-90 LOC). Future
  consumers: any iterator yielding heterogeneous JS values.
- Spec: the spec uses `(USVString or File)` union for FormData entry.
  WebIDL §3.7.10.2.
- Risk: union typing is a broader gap; for v1 just accepting an
  arbitrary `v8::Local<v8::Value>` (computed by user code) would
  cover FormData and any future similar iterator.

### B.6 `#[v8_const(NAME = value)]` for legacy IDL constants
- Consumers blocked: DOMException (A.2.4) — 25 legacy code constants.
  Future consumers: any IDL surface with `const` declarations
  (e.g. Event NONE/AT_TARGET/etc.; currently hand-rolled in
  `crates/runtime/src/web/dom/event.rs:34-40` as Rust constants but
  not exposed on the JS class).
- Pattern hand-rolled: a `&[(&str, u16)]` table + a `for (name, val)
  in TABLE { class_fn.set(...); proto.set(...); }` loop.
- Proposed macro feature: an impl-block-level repeatable
  `#[v8_const(NAME = u16)]` attribute. Codegen emits
  `class_fn.set(...)` AND `proto.set(...)` per WebIDL §3.7.5. Optional
  attribute for read-only / non-enumerable / non-configurable
  descriptor flavor.
- LOC delta if shipped: unblocks DOMException (-50 LOC); also enables
  Event.NONE / .AT_TARGET / .CAPTURING_PHASE / .BUBBLING_PHASE
  exposure that the JS-facing Event class is missing today.
- Spec: WebIDL §3.7.5 (interface constants).
- Risk: trivial codegen.

### B.7 WebIdlDict required-member enforcement
- Consumers blocked: `QueuingStrategyInit` (A.2.7) — and likely
  every WebIDL dict with required members. The Streams strategy
  classes are the documented case; ResponseInit's `status` is
  not strictly required (defaults to 200), but other dicts are.
- Pattern hand-rolled: model the field as `Option<f64>` and check
  `None` at the call site, dressing the converter error with the
  class name. See `strategies.rs:227-266` and the `WebIdlDict`
  reserved-hooks comment (`crates/runtime-macros/TODO.md:177-181`).
- Proposed macro feature: `#[webidl_required]` field-level attribute
  on `WebIdlDict`. The derive emits `if undefined { return Err(TypeError("…required…")) }`
  for that field's read step.
- LOC delta if shipped: -10 LOC per dict-with-required (3-5 dicts
  in v1).
- Spec: WebIDL §3.10 step 5 + §3.2.20.
- Risk: the macro derive lives in `webidl_dict.rs:21-275`. Adding the
  attribute is a 30-LOC edit.

### B.8 `#[v8_class]` install on prototype_template (vs resolved prototype)
- Consumers blocked: EventTarget (A.2.8) — currently hand-rolled
  precisely BECAUSE the macro installs methods on the resolved
  prototype object via `class_fn.set(prototype, ...)`. EventTarget's
  methods must live on `prototype_template` so derived classes
  picked up via `FunctionTemplate::inherit` see them.
- Pattern hand-rolled: each method goes via
  `proto_tmpl.set(key, fn_tmpl.into())` (event_target.rs:213-216).
- Proposed macro feature: `#[v8_class(install_on_prototype_template)]`
  flag on the impl block. Switches the install codegen from
  `proto.set(...)` to `proto_tmpl.set(...)`. AbortSignal (which
  inherits EventTarget via `#[v8_inherit]`) already does this
  correctly; the issue is the BASE class.
- LOC delta if shipped: unblocks EventTarget (-300 LOC of class
  template wiring) and any future spec base class with derived
  classes via `#[v8_inherit]`.
- Spec: WebIDL §3.7 inheritance chains (DOM §3.3 AbortSignal :
  EventTarget; future XHR : EventTarget, MessagePort : EventTarget,
  WebSocket : EventTarget — the last is already wired but goes
  via the macro's prototype-walk).
- Risk: the macro's `set_accessor_property` calls would also need
  to switch to `proto_tmpl.set_accessor_property`. ObjectTemplate
  vs Object are different V8 types but with parallel APIs.

### B.9 `WrapU16` newtype for default-case `unsigned short`
- Consumers blocked: CloseEvent.code (A.3.2 — `convert_unsigned_short_modulo`).
  Future: any IDL surface with default-case unsigned short.
- Pattern hand-rolled: 30 LOC fn doing NaN→0, truncate, modulo 2^16.
- Proposed macro feature: a `WrapU16` newtype mirroring the
  `EnforceRangeU64` / `ClampU16` pattern in
  `crates/runtime/src/webidl/`. Reader fn + macro detection by
  ident in `clamp_kind`-style helper. Optional broader scope: full
  set `WrapU8 / WrapU16 / WrapU32 / WrapI8 / WrapI16 / WrapI32`
  for completeness.
- LOC delta if shipped: -30 LOC (CloseEvent.code) + future-proof.
- Spec: WebIDL §3.2.10 ConvertToInt default case (the implicit case,
  no extended attribute).
- Risk: trivial; the algorithm is well-defined.

### B.10 `#[v8_method(returns_promise)]` for sync-but-Promise methods
- Consumers blocked: Blob.text/.arrayBuffer/.bytes; File.text/.arrayBuffer/.bytes
  (6 consumers, A.3.3). Future: any spec method that returns Promise
  but the value is computed sync (cached / pre-built).
- Pattern hand-rolled: each manually allocates a `PromiseResolver::new(scope).unwrap()`,
  gets the Promise, resolves immediately, returns the Promise local.
  6 copies of the same boilerplate.
- Proposed macro feature: `#[v8_method(returns_promise)]` attribute.
  User signature: `fn text(&self) -> Result<String, OpError>` (or any
  primitive return type). Macro wraps in Promise: success →
  resolved Promise, Err(OpError) → rejected Promise. Distinct from
  `#[v8_async_method]` (which is for true async via `state.spawned_ops`).
- LOC delta if shipped: -50 LOC across 6 sites.
- Spec: File API §3.3.7-9 (and any Promise-returning method that
  doesn't need to await).
- Risk: behavior must match WebIDL "Promise resolution" — for Promise<T>
  return per WebIDL §3.13.27 "convert a value to a Promise". The
  resolved-on-current-microtask vs next-microtask distinction is
  observable; spec mandates "immediate resolution" via SpeciesConstructor.
  The macro's existing `#[v8_async_method]` already gets this right
  via `state.spawned_ops`.

### B.11 `__zs_is_<Class>(scope, value) -> bool` cross-class brand check
- Consumers blocked: 4 sites today (A.3.5); many more after Request /
  Response migration (the body extraction path checks
  `is_blob_instance` / `is_form_data_instance` / `is_url_search_params`
  / `is_readable_stream_instance` etc.).
- Pattern hand-rolled: each consumer re-implements the prototype-chain
  walk using `obj.instance_of(scope, globalThis.Foo)` OR uses a
  partial check ("is internal field 0 an External") that's unsafe
  (the macro fixed this for own-method receivers in commit c95915e1
  but not for cross-class type queries).
- Proposed macro feature: emit `pub fn __zs_is_<Class>(scope, v: Local<Value>) -> bool`
  alongside `<Class>::install`. Body reuses the macro's existing
  `__brand_check_<Class>` — that function exists today (lines 547-621
  of `v8_class.rs`) but is private. Public re-export.
- LOC delta if shipped: -60 LOC across current 4 sites; eliminates
  the `instance_of(globalThis.Foo)` shape (which has its own footgun
  — if user code shadows `globalThis.Foo`, the check breaks).
- Spec: WebIDL §3.7 brand identity (matches the spec interpretation
  of `is X interface` better than `instance_of(globalThis.X)`).
- Risk: the macro's brand check uses prototype-chain walking against
  the cached prototype, which is the actual spec algorithm. Public
  re-export is a 1-line export.

### B.12 `#[v8_async_iterable]` for ReadableStream's `Symbol.asyncIterator`
- Consumer blocked: ReadableStream (A.1.3 — `[Symbol.asyncIterator]`
  manual install at `readable.rs:259-268`).
- Pattern hand-rolled: alias `[Symbol.asyncIterator]` to `values()`
  with a separate FunctionTemplate to set `set_class_name("values")`
  per WebIDL §3.7.10 spec for shared name.
- Proposed macro feature: a `#[v8_async_iterable(method = "values")]`
  attribute that mirrors `#[v8_iterable]` but for async iterators.
  Just emits the `[Symbol.asyncIterator]` alias.
- LOC delta if shipped: -10 LOC per consumer; ReadableStream is the
  one in v1, but EventSource / MessagePort / WebSocket-as-async-
  iterable are reasonable future consumers.
- Spec: WebIDL §3.7.10.5 `iterable<...>` async + `[Symbol.asyncIterator]`.
- Risk: minimal; trivial install codegen.

### B.13 `#[v8_class]` extension to project state from a wrapped struct
(`#[v8_state(Inner)]`)
- Consumers blocked: Request, Response (A.1.1, A.1.2) — both want to
  store `Box<RequestState>` / `Box<ResponseState>` in internal field
  0, but the macro requires `Box<Self>`.
- Pattern hand-rolled: define a unit `pub struct Request;` as the
  marker, hand-roll the entire class with state-pointer projection.
- Proposed macro feature: `#[v8_class]` accepts an `impl Self` over a
  unit struct PLUS a `#[v8_state(StateTy)]` impl-block-level
  attribute that names the actual state type. Codegen substitutes
  `*mut StateTy` for `*mut Self` in all internal-field reads.
  Constructor returns `StateTy`, getters/methods take `&StateTy` (or
  `&mut StateTy`).
- LOC delta if shipped: unblocks Request + Response (-1200 LOC
  combined).
- Spec: not spec-driven; pure ergonomic.
- Risk: the most invasive proposal in this report. The macro's brand
  slot is keyed by `class_ty` ident (the user struct name) — that
  stays. The internal-field cast type changes. Default constructor
  gets harder (`StateTy: Default` instead of `Self: Default`).

### Already in-flight (Tier 4 — separate agent)
For back-reference, NOT detailed here:
1. `WebIdlEnum(case_insensitive)` — for HashAlgo
2. `WebIdlEnum(silent_default)` — for RedirectMode/CredentialsMode/BinaryType
3. `WebIdlDict` member-level `reject_null` flag
4. `DictOrBool<T>` wrapper for `(X or boolean)` union types
5. `tc_scope` user-exception preservation in dict/enum extraction

---

## Summary

| Bucket                         | Count | Estimated LOC delta            |
|--------------------------------|-------|--------------------------------|
| Consumer migrations (Part A)   |    16 | -3,200 LOC across consumer code |
| New macro gaps (Part B)        |    13 | unblocks ~16 consumers, varies by gap |

Top-impact macro gaps (by # consumers unblocked):
- B.13 `#[v8_state(Inner)]` — unlocks Request + Response (A.1.1, A.1.2)
- B.3 `#[v8_constructor]` post-init / `args.this()` — unlocks 4-5
  streams classes (A.1.5, A.1.4)
- B.2 `same_object, project = field` — unlocks 5 SameObject getters
  across URL / Request / Response / AbortController (A.1.6)
- B.1 `#[v8_static_method]` — unlocks ~9 statics across 4 classes
  (A.1.7)
- B.4 `value_pairs(&mut self)` — unlocks Headers + URLSearchParams +
  FormData iterators (A.2.1, A.2.2, A.2.3)

Top-leverage migrations (by LOC saved):
- A.1.1 Request whole-class (-700) — blocked on B.13 + B.2
- A.1.2 Response whole-class (-550) — same blockers + B.1
- A.1.3 ReadableStream (-300) — blocked on B.3 + B.12
- A.2.1 Headers iterator (-350) — blocked on B.4
- A.1.6 SameObject getter migration (-200) — blocked on B.2
