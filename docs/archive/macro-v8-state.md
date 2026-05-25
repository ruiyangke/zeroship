# MAC-01 — `#[v8_state_marker(MarkerTy)]`: project V8 internal-field-0 from a separate state struct

- **Date:** 2026-05-04
- **Status:** **Shipped** — `#[v8_state_marker]` macro lives in `crates/runtime-macros/src/lib.rs` (`pub fn v8_state_marker`) and is preserved unchanged through the runtime-macros refactor (see `runtime-macros-refactor.md`). Document retained as design spec.
- **Tracking:** `crates/runtime-macros/TODO.md` Open section — once
  this proposal lands, the entry "Migrate hand-rolled
  `[SameObject]` getters to `#[v8_getter(same_object)]`"
  (TODO.md:265) becomes actionable for `Request.headers /
  signal` and `Response.headers`. URL.searchParams is independent
  (state-less today, no marker projection needed). Add a "Done"
  entry referencing this proposal's commit hash when the macro PR
  lands.
- **Owner / scope:** `crates/runtime-macros/src/v8_class.rs`, with one
  follow-up consumer migration in `crates/runtime/src/web/fetch/{request,response}.rs`.
- **LOC delta when both follow-up migrations land:** approximately
  **-535 LOC net** (Request -390, Response -145), revised from the
  brief's -1200 target after honest line-by-line accounting in §7.2
  and §7.3. The floor is set by the spec walks (~560 LOC of
  WebIDL §5.4 / §5.5 logic that migrates verbatim into the
  constructor body and can't be macro-eliminated). See §7.3.4 for
  the breakdown. The macro change itself adds roughly +120 LOC of
  codegen logic.

> **Polarity decision (Round 1 — settles old Open Question 1):** the
> attribute is named `#[v8_state_marker(MarkerTy)]` (state-shaped impl,
> marker named explicitly). The polarity question raised in v1 is
> answered in §2.2 and removed from the open-question list. The doc
> title above and every code snippet below uses this spelling
> consistently.
>
> **Why this polarity:** the existing codebase already uses state-shaped
> impls de facto. `crates/runtime/src/web/dom/close_event.rs:151-204`
> declares `#[v8_class] #[v8_inherit(Event)] impl CloseEventState { ... }`
> — `CloseEventState` (the state struct) IS the impl receiver, with no
> separate marker type. `crates/runtime/src/web/dom/abort_signal.rs:123-125`
> declares `#[v8_class] #[v8_inherit(EventTarget)] impl AbortSignal { ... }`
> where `AbortSignal` is the state struct (its name happens to match the
> JS class name, but it's the state). So existing classes ALREADY use
> "the impl receiver IS the boxed state". The migration target — Request
> and Response — is the anomaly: they use `Request` / `Response` as
> JS-only marker units while the state lives in `RequestState` /
> `ResponseState`. This proposal aligns the migration target with the
> existing convention by letting the user keep both names AND keep the
> impl on the state.

---

## 1. Problem framing

### 1.1 What today's `#[v8_class]` mandates

Every existing `#[v8_class]` consumer pairs the JS-facing class with a
single Rust type that **is** the boxed state. `crates/runtime-macros/src/v8_class.rs:1731-1757`
emits:

```rust
let __boxed = Box::new(__instance);          // __instance: #class_ty
let __raw_ptr = Box::into_raw(__boxed);      // *mut #class_ty
let __ext = v8::External::new(scope, __raw_ptr as *mut c_void);
__this.set_internal_field(0, __ext.into());
// finalizer drops Box::from_raw as *mut #class_ty
```

The same `#class_ty` ident drives:

- the brand-check prototype slot (`v8_class.rs:480-481, 510-511, 547-621`),
- every method/getter/setter callback's pointer recovery
  (`v8_class.rs:1141-1157, 1281-1298, 1413-1423, 1536-1550`),
- the constructor's `make_instance`/`Box::into_raw` site
  (`v8_class.rs:1640-1675, 1714, 1731-1751`),
- the async future's per-poll re-acquisition (`v8_class.rs:1391-1473`),
- the GC-finalizer drop type (`v8_class.rs:1746-1750`).

The `class_ty` is `Self`. `Self` is the `Box<...>` content. There is no
escape hatch.

<!-- Revised in round 3 (addressing MINOR #9): the four arguments are
     now grouped into "Storage architecture" (1, 3) and "Construction
     surface" (2, 4) for clarity. -->

### 1.2 What Request and Response actually want

Both fetch classes already separate the JS-facing marker from the
storage struct:

```rust
// crates/runtime/src/web/fetch/request.rs:55-107
pub struct RequestState { /* RefCell<...> for every spec field */ }
impl Default for RequestState { /* spec defaults */ }

// crates/runtime/src/web/fetch/request.rs:116
pub struct Request;                         // unit marker
impl BodyMarker for Request { /* CLASS_LABEL */ }
impl Body     for Request { /* body_state, content_type via state_ptr */ }
```

The `Request` unit type is what would carry `#[v8_class] impl Request`,
but the box stored in internal field 0 is `Box<RequestState>`.
`Response` mirrors this exactly (`response.rs:57-92`).

**Storage architecture (constraints on the boxed state's *type*):**

1. **Body trait dispatch needs `Request` as the trait-dispatch type AND
   `RequestState` as the storage type** (`request.rs:118-152`,
   `response.rs:89-124`). `install_body_methods::<Request>` and the
   WPT-shaped consumer methods (`text/json/arrayBuffer/bytes/blob/formData`)
   are keyed by the marker (`Request` / `Response`), while the actual
   storage projects through `Body::body_state` to a `&BodyImpl` borrowed
   from the state struct's `RefCell<BodyImpl>`.
2. **Immutable-after-disturbed semantics require the state to be
   constructible incrementally before being committed.** The
   constructor's spec walk has 43 steps, many of which conditionally
   throw. Building as `Box<Self>` from the start would force allocation
   on every speculative attempt. Same shape in `response.rs:436-567`.

**Construction surface (constraints on the boxed state's *construction
sequence*):**

3. **Clone semantics need to read from a *different* Request's state
   while building the output's own state.** See `request.rs:355-385`:
   when copying scalar fields out of an input Request, the constructor
   reads `other: &RequestState = unsafe { &*raw }` from the input
   wrapper while building the output's own state struct, *then* boxes
   the new state at the very end (`request.rs:733-746`). Mixing the
   JS-facing marker into the storage struct would force every input-
   copy site into trait-dispatch indirection.
4. **Kernel fast-path builders (`build_kernel_request`,
   `build_kernel_response`) construct the state struct without going
   through the WebIDL constructor at all** (`request.rs:780-846`,
   `response.rs:296-367`). Forcing `Self == State` would merge two
   distinct construction surfaces with subtly different validation
   contracts.

The macro forces `Self` to be both the JS-side identity (used for
`BodyMarker::CLASS_LABEL`, instance-of checks, etc.) and the boxed
state. Today both files escape that by hand-rolling the entire
`#[v8_class]` codegen — including the brand check, internal-field
finalizer, every getter, the must-new prologue, and the async/sync
method install table. That hand-roll is now ~1250 LOC of mostly
boilerplate.

### 1.3 Why this is the right macro change to make now

- **Two large consumers, one shape.** Request and Response are the
  only classes today that own a sizeable internal state separate from
  the marker. Migrating them is the test that the abstraction works.
- **Doesn't touch the runtime hot path.** The substitution is type-
  level inside the macro; it changes neither V8 op count, allocation
  count, nor pointer arithmetic in the emitted code.
- **Unblocks downstream MAC-08** (`#[v8_getter(same_object, project = field)]`):
  once state is a separate type, `same_object` can cache directly on a
  `RefCell<Option<Global<Object>>>` field rather than a V8 private
  symbol — closing the last hand-roll gap on `Request.headers /
  signal` and `Response.headers`.
- **Other open MAC items reuse the same machinery.** MAC-02
  (`post_init` hook with `&Self`) and the streams classes (Reader /
  Writer / TransformStream) all want to take `&self` against state
  that's distinct from the wrapper marker. Picking a stable
  `#[v8_state]` shape now keeps those follow-ups additive.

<!-- Added in round 3 (addressing MINOR #10): industry comparison
     in its own subsection rather than nested in §2.2's decision rationale. -->

### 1.4 Industry comparison

| Project | Per-class state model | Brand check | Hot-path impl |
|---|---|---|---|
| zeroship (this codebase) | `Box<State>` in V8 internal field 0; finalizer drops on GC | Prototype-chain walk against cached `Foo.prototype` | Macro `#[v8_class]` codegen — what this proposal extends |
| Deno (`deno_core::op2`) | Resource table indexed by `OpState`; per-class storage via `v8::Local<v8::Object>` resources | Resource ID lookup | Free `#[op2]` function, not bound to a class |
| Cloudflare workerd | C++ template-instantiated classes for built-in IDL bindings (capnp-derived schemas drive the templates); storage in C++ instance, JS sees fields via accessors | Prototype check at the C++ layer | Templates instantiated at compile time |
| `wasm-bindgen` | Pointer-as-integer in JS userland; pointer table in wasm-bindgen runtime | Type-id check on the integer's pointer-table entry | Free wasm fns, not V8-resident |

**Why we don't take Deno's approach:** the OpState resource table is
a shared-table indirection — every per-class state read goes through
a HashMap lookup. We keep per-instance state in V8 internal field 0
for direct pointer access. Our model is closer to workerd's
JsgClass and to WebIDL's `[[Slots]]`, but at the macro level rather
than C++ template level.

**Why we don't take wasm-bindgen's approach:** we need WebIDL-shape
brand checks (`instanceof Foo` prototype walks), which require
proper V8 prototype-chain machinery. Pointer-as-integer in JS
userland makes brand checks O(N) over a runtime-managed pointer
table; V8's prototype-walk is O(depth) on the JS-native chain.
Also, V8's GC / finalization integrates with internal fields but
not with userland integers — using userland would leak memory on
isolate teardown.

---

## 2. Design adequacy

### 2.1 Surface

```rust
#[v8_class]
#[v8_state(RequestState)]
impl Request {
    #[v8_constructor]
    fn new(input: v8::Local<v8::Value>, init: v8::Local<v8::Value>)
        -> Result<RequestState, OpError> { /* ... */ }

    #[v8_getter]
    fn method(&self) -> String { self.method.borrow().clone() }
    //          ^^^^^ this `self` is `&RequestState`, NOT `&Request`.
    //          See §2.2 for the receiver-binding decision.

    #[v8_method]
    fn clone(&self, scope: &mut v8::PinScope) -> v8::Local<v8::Object> { /* ... */ }
}
```

- `#[v8_class]` continues to take the JS-facing marker type via the
  `impl <Marker>` syntax. No change to its existing surface.
- `#[v8_state(StateTy)]` is a new impl-block-level attribute. When
  present, the macro substitutes `*mut StateTy` for `*mut Self` in
  every internal-field cast, the finalizer's drop type, and the
  per-method receiver. When absent, the macro behaves exactly as
  today (§5).
- The constructor's return type is `StateTy` (or
  `Result<StateTy, OpError>`), not `Self`. The macro boxes `StateTy`
  into V8 internal field 0.
- Receiver methods (`#[v8_method]`, `#[v8_getter]`,
  `#[v8_setter]`, `#[v8_async_method]`) still write `&self` /
  `&mut self`, but the macro routes the receiver to `StateTy`, not the
  marker type. Static methods (`#[v8_static_method]`, MAC-07, future)
  will not have this dispatch shift.

<!-- Revised in round 1 (addressing CRITICAL #2 partially, MAJOR #1, MAJOR #2,
     MINOR #2, and consolidating the meandering Option B/B-revised/B-final/C
     analysis from v1): three options crisply, one decision, one syntax. -->

### 2.2 Receiver-binding decision

**Three options were considered:**

| Option | Impl receiver | Method `&self` is | Cost |
|---|---|---|---|
| A | `impl Marker` | `&Marker`, but macro reinterprets as `&StateTy` | Type-checker lies — typos surface as confusing errors against `Marker` rather than `StateTy` |
| B (recommended) | `impl StateTy` + `#[v8_state_marker(Marker)]` | `&StateTy`, honestly | One extra attribute |
| C | `#[v8_state_class(state = StateTy, marker = MarkerTy)]` (new attribute family) | `&StateTy` | Churn — every existing `#[v8_class]` user is unaffected, but the new family is non-orthogonal with `#[v8_iterable]`, `#[v8_inherit]`, etc. |

**Recommendation: Option B.**

```rust
#[v8_class]
#[v8_state_marker(Request)]              // JS class identity
impl RequestState {                       // Rust receiver identity
    #[v8_constructor]
    fn new(...) -> Result<Self, OpError> { ... }

    #[v8_getter]
    fn method(&self) -> String { self.method.borrow().clone() }
    //         ^^^^^ &RequestState. No lying.
}

// Marker type must exist as a Rust type. Two valid shapes:
//
//   pub struct Request;                        // unit marker (Request/Response use this)
//   // OR a separate type with its own impls (e.g., Body trait dispatch).
```

**The macro uses `Request` (the marker) for** (cross-ref to substitution
table §4.1):
- the brand-check prototype slot (`__BrandSlot_Request`, row 1),
- the install-slot type (`__InstallSlot_Request`, row 2),
- the install fn's enclosing impl: `impl Request { fn install(...) }` (row 4),
- `set_class_name("Request")` so `Request.name === "Request"` in JS (row 5),
- `Symbol.toStringTag` default (row 21),
- must-new error message wording: `"Failed to construct 'Request': ..."` (row 20),
- iterable companion install (row 22) — when both `#[v8_iterable]` and
  `#[v8_state_marker]` apply, the iterator companion is on the marker.

**The macro uses `RequestState` (the receiver) for**:
- the box payload (`Box<RequestState>` in V8 internal field 0, rows 6, 7, 8),
- every internal-field cast (rows 10, 12, 14, 17),
- the finalizer's drop type (`drop(Box::from_raw(... as *mut RequestState))`, row 9),
- the receiver type in method callbacks (rows 11, 13, 15, 18).

**Why Option B over A:** receiver-type honesty matters under typos and
under `#[v8_inherit]`. Under inheritance, the parent's prototype-walking
brand check uses the marker; the parent's getters cast through internal
field 0 to the parent's state type. With Option A's lie, error messages
on a typo in a child method would refer to the *marker* type (which has
no fields), confusing the user about why `self.foo` doesn't exist. With
Option B, errors point at `RequestState`, which is what's actually
behind the pointer.

**Why Option B over C:** orthogonality. `#[v8_state_marker]` composes
with `#[v8_inherit(...)]`, `#[v8_inherit_intrinsic]`, `#[v8_iterable]`,
`#[v8_to_string_tag]` — each lives on the impl block as an independent
attribute. A single `#[v8_state_class]` super-attribute would either
duplicate that surface or introduce a third, parallel attribute family.

**Why this matches the de-facto codebase convention:** existing classes
like `CloseEventState` (`crates/runtime/src/web/dom/close_event.rs:151`)
and `AbortSignal` (`crates/runtime/src/web/dom/abort_signal.rs:123`)
already write `#[v8_class] impl <state-struct>` — the impl receiver IS
the boxed state. They don't use a marker because their state struct
already has the right name. Request/Response are the anomaly — they
need *both* a name (`Request`) for JS identity *and* a structurally
distinct state name (`RequestState`) for the unit-marker / state-struct
split that the Body trait + the input-Request copy semantics require
(§1.2). Option B lets them keep both. (Industry comparison moved to
§1.4 in round 3 for cleaner section structure.)

### 2.3 Constructor handling

Two cases.

**Case 1 — user-defined `#[v8_constructor]`.**
Today: returns `Self`. Macro boxes via `Box::new(__instance)`.
With `#[v8_state_marker(M)]`:
- the user's body returns `Self == StateTy` (e.g., `RequestState`),
- the macro boxes `Box::new(__instance: StateTy)` and stores via
  `External` exactly as today,
- `Result<Self, OpError>` continues to work (the existing `make_instance`
  branch in `v8_class.rs:1640-1675` is type-agnostic — it constructs
  via `<#class_ty>::#ctor_name(...)` and types the result as
  `let __instance: #class_ty = ...`. We change `#class_ty` to the
  state-typed call site and the `__instance` type to `StateTy`).
- Per WebIDL §3.7.1, `is_construct_call` is still required; `must_new`
  prologue (`v8_class.rs:1606-1622`) remains keyed on the marker name
  for the user-visible error message.

**Case 2 — no user constructor, `Default`-derived fallback.**
Today: `<#class_ty as Default>::default()` (`v8_class.rs:1714`). With
`#[v8_state_marker(M)]`: `<#state_ty as Default>::default()`. We
require `StateTy: Default` instead of `Self: Default`. **This IS a
breaking change for any existing class that today relies on the
default-derived constructor and uses `#[v8_state_marker]`.** Mitigation:
the change is opt-in via the new attribute; existing classes (no
`#[v8_state_marker]`) stay on the `Self: Default` requirement. There
are zero classes today that would be auto-migrated, so the breakage
window is empty until a consumer opts in. Both `RequestState` and
`ResponseState` already implement `Default` (`request.rs:84-107`,
`response.rs:68-81`), so the migration's mechanical bound is met.

### 2.4 Brand check

The `#[brand_slot_ty]` and `#[brand_check_fn]` (`v8_class.rs:480-481,
547-621`) cache `<MarkerTy>.prototype` and walk the receiver's
prototype chain looking for it. The state type is **never** the
keying identity — even today, the brand check uses the
FunctionTemplate's prototype, not the boxed type. So with
`#[v8_state_marker]` the brand check is unchanged; we just rename
the slot type after the marker, not the state. (The slot type names
are emitted as `__InstallSlot_<Marker>` and `__BrandSlot_<Marker>`,
and the cached `Foo.prototype` is the marker's prototype — exactly
what we want.)

### 2.5 Drop / finalization

`gen_box_and_install_finalizer` (`v8_class.rs:1731-1757`):
```rust
let __boxed = Box::new(__instance);
let __raw_ptr = Box::into_raw(__boxed);
// ...
let __weak = v8::Weak::with_guaranteed_finalizer(scope, __this,
    Box::new(move || { unsafe { drop(Box::from_raw(__raw_addr as *mut #class_ty)); } }));
```

With `#[v8_state_marker(M)]`: replace `#class_ty` with `#state_ty` in the
finalizer's drop type. Same allocation, same lifetime, same V8 weak
shape.

### 2.6 Per-method receiver

Every method/getter/setter callback in the macro recovers
`__instance: &mut #class_ty` from the External pointer. Substitute
`#state_ty`. Receiver shape stays `&self` / `&mut self` because Rust
desugars those against the *enclosing* `impl Self` — and in Option B
the enclosing impl is `impl StateTy`, so the substitution is correct
*by construction*.

<!-- Revised in round 1 (addressing CRITICAL C2(b)): the marker-naming
     argument is necessary but not sufficient. Two distinct classes with
     same-named markers in different modules would still collide. We now
     qualify the symbol name by `module_path!()` to make collisions
     impossible. -->

### 2.7 `[SameObject]` getter cache

`gen_same_object_getter_callback` (`v8_class.rs:1218-1319`). The cache
key is a V8 Private symbol named `__zs_same_object_<#class_ty>_<method>`
(`v8_class.rs:1237`). With Option B:

- The macro renames the symbol to
  `__zs_same_object_<module_path>::<MarkerTy>_<method>`,
  with `module_path` resolved via `module_path!()` at codegen time.
  This guarantees no two classes — even with same-named markers in
  different modules — share a Private symbol. Specifically:
  - `__zs_same_object_zeroship_runtime::web::fetch::request::Request_headers` (Request.headers)
  - `__zs_same_object_zeroship_runtime::web::fetch::response::Response_headers` (Response.headers)

  V8's `Private::for_api` interns the name; distinct names yield distinct
  Privates. Cross-module shadowing is impossible.

- The `__instance` recovery casts to `*mut <StateTy>` and the user
  method returns `Global<Object>` from a `&StateTy` receiver.

- *Emit detail:* the macro reads `module_path!()` via a const at the
  callback's site (`module_path!()` is a built-in macro available in
  any expansion). The previous v1 proposal naming
  `__zs_same_object_<MarkerTy>_<method>` is insufficient against
  cross-module collisions; the qualified form replaces it.

- **Cache-vs-state-field interaction (added in round 1, addresses
  critic CRITICAL #3).** The macro's cache lives on a V8 Private
  symbol attached to the wrapper Object — it is *not* the same slot
  as a `RefCell<Option<v8::Global<v8::Object>>>` field on the state
  struct. Today's hand-rolled `Request.headers` getter caches on the
  state field (`state.headers: RefCell<Option<Global<Object>>>`); the
  macro caches on a V8 Private. Two storage sites for the same
  identity-cache is wasteful AND a source of inconsistency (e.g.
  `build_kernel_request` populates `state.headers` eagerly; a first
  call to the macro-generated getter would never populate the V8
  Private if it short-circuits via state.headers). The migration
  resolves this by **deleting the state-field cache for `headers` and
  `signal`**, relying on the macro's V8 Private symbol as the only
  storage. The state struct keeps the field type
  `RefCell<Option<v8::Global<v8::Object>>>` for transitional
  compatibility ONLY during the constructor (which still wants to
  hold the Headers Global until it can install it via
  `set_private`); after construction the field is `None` and the V8
  Private is the source of truth.

  Concretely, the migrated `Request::headers` getter does:
  ```rust
  #[v8_getter(same_object)]
  fn headers<'s>(
      &self,
      scope: &mut v8::PinScope<'s, '_>,
  ) -> v8::Global<v8::Object> {
      // The constructor stashed a Global on state.headers. Read it
      // out once here on the first cache-miss; the macro's V8 Private
      // becomes the source of truth thereafter.
      if let Some(g) = self.headers.borrow().as_ref() {
          return g.clone();
      }
      // Lazy mint (only the kernel fast path can reach this branch
      // because build_kernel_request pre-populates state.headers).
      let h = crate::headers::build_empty(scope);
      v8::Global::new(scope, h)
  }
  ```
  And `build_kernel_request` continues to set `state.headers =
  Some(...)`. The `same_object` cache hit is observed by JS as the
  same Object-identity across reads, satisfying `[SameObject]`.

### 2.8 Async methods

`gen_async_method_callback` (`v8_class.rs:1379-1500`). Critical
substitutions:
- `__raw_addr: usize` and the per-poll cast `__instance: &#class_ty =
  unsafe { &*(__raw_addr as *mut #class_ty) }` (`v8_class.rs:1473`):
  replace `#class_ty` with `#state_ty`.
- The future captures `__wrapper_global` (the JS wrapper, not the
  state). `wrapper_global`'s liveness still pins the box, *because the
  V8 finalizer was registered against the wrapper Object* — and the
  finalizer drops `Box::from_raw(... as *mut #state_ty)`. The keepalive
  argument in `gen_async_method_callback`'s doc comment
  (`v8_class.rs:1364-1378`) survives the substitution: the wrapper
  pins the same allocation; only the type name on the cast changes.
- The compile-time guard rejecting `&mut self` async methods
  (`v8_class.rs:328-337`) is unchanged. Re-acquiring `&Self`-shaped
  references per poll is sound for `&StateTy` for the same reason it's
  sound for `&Self`: shared references are aliasable; the user is
  responsible for `Cell` / `RefCell` on mutating state.

<!-- Revised in round 2 (addressing Missing Concept #5): the iterable
     companion class's NAME is settled (uses marker, not state); the
     iterable codegen is invoked with marker_ty, not class_ty. -->

### 2.9 Iterable (`#[v8_iterable]`)

Expanded in `crates/runtime-macros/src/v8_iterable.rs`. The companion
class is named after the **marker** (`<MarkerTy>Iterator`), NOT the
state. So `Headers.entries()` returns a `HeadersIterator` (today and
forever), and a hypothetical `RequestBody.entries()` (under state
projection) would return `RequestBodyIterator`, NOT
`RequestBodyStateIterator`. Concretely:

- The macro invokes `v8_iterable::generate(marker_ty, attr)` instead
  of `v8_iterable::generate(class_ty, attr)`. Row 22 in §4.1 reflects
  this.

- The iterator companion's own state is keyed by the marker name in
  emitted idents — `__InstallSlot_HeadersIterator`,
  `__BrandSlot_HeadersIterator`, etc. Iterator state is its OWN struct
  emitted by `v8_iterable.rs`; iterators don't compose
  `#[v8_state_marker]` in v1.

- Future: if a state-projected class adds `#[v8_iterable]`, the
  iterator companion uses `<MarkerTy>Iterator` for the JS class name
  AND for its own state struct (a single struct with `Self == State`
  internally). No nested state projection.

We do NOT propagate `#[v8_state_marker]` into iterable codegen in v1 —
iterators are simple enough to keep the `Self == State` shape inside
the companion. Future work: if FormData migrates to a state-split
shape, iterators may want their own `#[v8_state_marker]`. Out of scope
here.

<!-- Revised in round 1 (addressing CRITICAL C5): the existing
     `#[repr(C)]` Event-as-first-field pattern in CloseEventState is
     load-bearing. State projection on a subclass would shift layout
     and break the parent's getter casts. We now show this works
     under Option B because the parent's getters cast to the parent's
     state type (Event), and CloseEventState's `event: Event` first
     field provides that layout-compat — same as today. -->

### 2.10 `#[v8_inherit]` / `#[v8_inherit_intrinsic]`

`gen_install` emits:
```rust
__ctor_tmpl.inherit(<#base>::install(scope));   // v8_class.rs:858-862
```

`#base` is a syn::Path — typically `super::event_target::EventTarget`
in `abort_signal.rs:124`. The base class is itself a `#[v8_class]`,
which (post-this-change) might or might not also use
`#[v8_state_marker]`. The two are independent: `inherit` operates on
the FunctionTemplate identity (the install slot), not the boxed state
type.

#### Worked example: CloseEvent : Event

`crates/runtime/src/web/dom/close_event.rs:37-48` declares:

```rust
#[repr(C)]
pub struct CloseEventState {
    /// Inherited Event state — at offset 0 (#[repr(C)]) so
    /// `*mut CloseEventState` is layout-compatible with `*mut Event`.
    pub event: Event,
    pub was_clean: Cell<bool>,
    pub code: Cell<u16>,
    pub reason: RefCell<String>,
}

#[v8_class]
#[v8_inherit(super::event::Event)]
#[v8_to_string_tag = "CloseEvent"]
impl CloseEventState { ... }
```

Today: the box stored in internal field 0 of a CloseEvent JS wrapper is
`Box<CloseEventState>`. The Event-prefix layout means `*mut
CloseEventState` and `*mut Event` are pointer-equivalent for accessing
`event.*` fields. The parent's getters (Event::type, Event::bubbles,
…) cast through the External as `*mut Event` and read the embedded
fields. **This works today because Self == BoxedState for both
parent and child** — Event is a `#[v8_class] impl Event` over its own
fields; CloseEventState's `event: Event` first-field guarantees layout-
compat.

**Under Option B (state-projection):**

| Combination | Parent storage | Child storage | Layout-compat needed? |
|---|---|---|---|
| Parent no-attr, child no-attr (TODAY) | `Box<Parent>` | `Box<Child>` (with `event: Parent` first field) | Yes — child's `*mut` casts to `*mut Parent` work via `#[repr(C)]` |
| Parent no-attr, child `#[v8_state_marker(M)] impl ChildState` | `Box<Parent>` | `Box<ChildState>` (with `event: Parent` first field) | Yes — same as today; the macro's child-state cast is to `*mut ChildState`, parent's getters cast to `*mut Parent` (still finds the Event-prefix) |
| Parent `#[v8_state_marker(P)] impl ParentState`, child no-attr | `Box<ParentState>` | `Box<Child>` (with `parent_state: ParentState` first field) | Yes — child's `*mut Child` and parent's `*mut ParentState` are layout-compat via `#[repr(C)]` |
| Both `#[v8_state_marker]` (subclass owns its own state, parent owns its own state) | `Box<ParentState>` | `Box<ChildState>` | Same — `#[repr(C)]` + `parent_state: ParentState` first field |

**The substitution preserves the layout invariant.** What changes is
the *name* the macro casts to (`*mut CloseEventState` today, `*mut
ChildState` under projection), not the *layout* it relies on. As long
as the child's state struct has the parent's state struct as its first
`#[repr(C)]` field — the existing convention — the parent's getters
continue to find their fields. Critic round-1 C5 is resolved by the
fact that the macro's child-getter cast type changes (rows 10/12/14/17
in §4.1), but the parent's getter cast type does NOT change (the
parent's macro emits its own callbacks keyed off the parent's state
type, which the parent owns). No code today combines `#[v8_state_marker]`
on a child with `#[v8_inherit]` of a class that uses `#[v8_state_marker]`,
but the contract is well-defined for that future case.

**v1 scope decision.** v1 supports any combination of (parent: attr,
no-attr) × (child: attr, no-attr). The `#[repr(C)]` first-field
convention is the user's responsibility — same as today.

### 2.11 Reentrancy guard

`gen_reentry_guard` (`v8_class.rs:1025-1074`). The guard's set is
keyed by `__ext.value() as usize`, which is the External's pointer
value — same address before and after the substitution. Per-method
thread-local set, per-instance entry — unchanged.

<!-- Added in round 1 (addressing Missing Concepts #1, #3, #4):
     Body trait + install_body_methods, debug-time sanity checks. -->

### 2.12 `BodyMarker` and `install_body_methods` continue to work as-is

`crates/runtime/src/web/fetch/body/consumers.rs` defines:

```rust
pub trait BodyMarker { const CLASS_LABEL: &'static str; }
pub fn install_body_methods<M: BodyMarker + Body>(scope, proto) { ... }
```

For `Request`: `impl BodyMarker for Request { CLASS_LABEL = "Request" }`,
and `impl Body for Request { fn body_state(scope, this) -> &BodyImpl }`.
The `Body` trait's body materialization reads internal field 0 and
casts to `*mut RequestState`. After migration, the macro emits the
same cast in its method callbacks. **Both casts (Body trait dispatch
AND macro callback) target `*mut RequestState`**, so they
co-exist soundly.

The `install_body_methods::<Request>(scope, proto)` call is OUTSIDE
the macro's emit. It runs after `Request::install` returns, called
explicitly from `setup_globals` (or, when MAC-02 lands, from a
`#[v8_post_install]` hook). The macro doesn't know about
`BodyMarker` or `Body`, and shouldn't.

This is intentional: the macro is the kernel for class wiring; the
body trait is a user-space mixin. Keeping the boundary clean lets us
add body-like mixins for future classes (e.g., a `Streamable` mixin
for ReadableStream's transformers) without macro changes.

### 2.13 Optional debug-time integrity check

The macro could, under `cfg(debug_assertions)`, emit a TypeId-based
sanity check at every cast site:

```rust
// Emitted only under debug_assertions: verify the boxed type
// matches what we expect. Catches a class of UB where a getter is
// somehow installed on the wrong prototype and ends up casting a
// Box<Foo> as Box<Bar>.
#[cfg(debug_assertions)]
{
    let __observed_typeid = unsafe { &*(__ext.value() as *const __TypeIdMarker) }.id;
    assert_eq!(__observed_typeid, std::any::TypeId::of::<#state_ty>(),
        "v8_class: internal field 0 type mismatch ...");
}
```

**Decision (v1: skip).** Today's macro doesn't do this either, and
the brand-check (which walks the prototype chain for the marker's
prototype, not the box type) catches cross-class calls before the
unsafe deref. A TypeId tag would require an extra word per box and
complicate the finalizer. Defer until a real bug motivates it.

---

## 3. Spec correctness

### 3.1 V8 internal-field-0 semantics

The V8 contract for `set_internal_field(0, External::new(ptr))` is
that the slot stores an opaque pointer; the runtime is responsible
for casting it back to the correct type. V8 does not know — and does
not care — what `T` the box wraps. From the V8 side, our substitution
is invisible.

The `with_guaranteed_finalizer` closure (`v8_class.rs:1743-1751`) runs
on GC-of-the-wrapper or isolate teardown. The finalizer captures
`__raw_addr: usize` and casts back to `*mut T`. As long as the cast
type matches `Box::into_raw(Box::new(T))`, the drop is sound. Our
substitution pairs `Box::into_raw(Box<StateTy>)` with
`Box::from_raw(*mut StateTy)` symmetrically.

<!-- Revised in round 4 (addressing MINOR #14): explicit cite to
     WebIDL §3.7.4 and §3.7.5 for completeness. -->

### 3.2 WebIDL §3.7 — interfaces

WebIDL §3.7 specifies that an interface's instance is associated with
its class identity through the prototype chain (`[[Prototype]] ===
Foo.prototype`). Specifically:
- §3.7.4 "Internal slots" defines the per-instance state mechanism
  abstractly. The spec only requires that named slots are accessible
  via the prototype's accessor functions; storage is unspecified.
- §3.7.5 "Interface prototype object" specifies that the interface's
  prototype object is the identity used by `instanceof Foo` checks.
  Our brand-check walks `obj`'s `[[Prototype]]` chain looking for the
  cached `Foo.prototype` Object — exactly the predicate WebIDL
  describes (`v8_class.rs:547-621`).

The spec does not say anything about the runtime's backing storage —
it's purely a JS-side contract. Per the task brief: "the spec doesn't
dictate impl shape, so don't quote it as a constraint." We do not.

<!-- Added in round 3 (addressing Missing Concepts #1, #2): panic
     semantics during construction and finalizer-registration ordering
     are now explicit. -->

### 3.3 What the substitution does NOT touch

- WebIDL `[SameObject]` semantics: still "same JS Object across reads"
  — see §2.7.
- WebIDL `[NewObject]` semantics for `clone()` and similar: the user
  method's return type drives this; macro is agnostic.
- Iterator default protocol (§3.7.10.2): see §2.9.
- WebIDL §3.7.1 must-new: marker-named TypeError message; see §2.3.
- WebIDL ByteString / USVString conversion: handled by the param-extraction
  layer (`gen_extract` in the parent crate's `lib.rs`); state type is
  not visible there.

### 3.4 Panic semantics during construction

If `RequestState::new(...)` panics mid-way (e.g., a third-party
parser panics on a malformed input), Rust's panic runtime cannot
unwind through V8's C++ frames cleanly — the result on Linux is
"fatal runtime error: failed to initiate panic, error 5" + SIGABRT,
same as today's `request_constructor_callback` panicking. **Migration
preserves this behavior.** The macro's `gen_constructor_callback`
body is `let __instance: #state_ty = match <#state_ty>::#ctor_name(...) { Ok(__v) => __v, Err(...) => ... }`;
a panic inside the user's `#ctor_name` aborts the same as the hand-
roll. The `#[v8_constructor]` user code SHOULD return
`Err(OpError::...)` rather than `panic!` for any expected error path.

### 3.5 Finalizer-registration order invariant

The macro's `gen_box_and_install_finalizer` (v8_class.rs:1722-1748)
emits this sequence:

```rust
let __ext = v8::External::new(scope, __raw_ptr as *mut c_void);
__this.set_internal_field(0, __ext.into());     // (1)
let __weak = v8::Weak::with_guaranteed_finalizer(scope, __this, ...);  // (2)
::std::mem::forget(__weak);
```

**Order matters.** (1) MUST run before (2). If (2) ran first, V8 could
GC the wrapper between (2) and (1) — the finalizer would fire on a
wrapper without an installed External, and the closure would
double-free or crash trying to drop nothing.

The proposed substitution preserves this order (the `__ext` build
and `set_internal_field` happen in the same template; only the type
ascription `*mut #state_ty` changes). v1 carries this forward
unchanged. Smoke-test coverage: existing `v8_class_smoke.rs` already
exercises GC of fresh wrappers; `state_finalizer_drops_state_type`
in §6.1 row 9 adds the state-projected variant.

---

## 4. Codegen feasibility

<!-- Revised in round 1 (addressing CRITICAL #1, MINOR #5):
     1. Distinguished "emitted-token" sites from "type-inference-only" sites
        with explicit `[change]` / `[type-inference]` flags.
     2. Re-grounded each row's line number against the actual master
        @ 4c41db3 (verified by reading v8_class.rs, post-rebase).
     3. Removed row 21's reference to a non-existent `set_to_string_tag`
        function — the tag is set inline at v8_class.rs:932-940. -->

### 4.1 Substitution table

Every site in `crates/runtime-macros/src/v8_class.rs` that emits
`#class_ty` as a *token*. Grouped by what becomes the marker
(`#marker_ty`, the JS-identity ident) vs. what becomes the state
(`#state_ty`, the boxed-payload ident). Rows are tagged
`[emit-change]` (the `quote!` block must change) or `[invariant]`
(the `quote!` block stays the same; only the type that's
interpolated changes meaning when `state_ty != class_ty`).

> Line numbers verified against `crates/runtime-macros/src/v8_class.rs`
> at HEAD on branch `worktree-agent-a737cfe3` (master tip @ 4c41db3,
> file at 1939 LOC). Token quotes are exact.

| # | Site | Lines | Today's `quote!` | After (Option B) | Tag |
|---|---|---|---|---|---|
| 1 | brand-slot type ident | 479 | `format_ident!("__BrandSlot_{}", class_ty)` | `format_ident!("__BrandSlot_{}", marker_ty)` | [emit-change: replace `class_ty` → `marker_ty`] |
| 2 | install-slot type ident | 478 | `format_ident!("__InstallSlot_{}", class_ty)` | `format_ident!("__InstallSlot_{}", marker_ty)` | [emit-change: replace `class_ty` → `marker_ty`] |
| 3 | brand-check fn ident | 480 | `format_ident!("__brand_check_{}", class_ty)` | `format_ident!("__brand_check_{}", marker_ty)` | [emit-change] |
| 4 | install fn impl block | 622-625 | `impl #class_ty { #install }` | `impl #marker_ty { #install }` | [emit-change] |
| 5 | `set_class_name` literal | 689, 902-903 | `class_name_str = class_ty.to_string()` | `class_name_str = marker_ty.to_string()` | [emit-change] |
| 6 | constructor user-call let-type ascription | 1641, 1663 | `let __instance: #class_ty = match <#class_ty>::#ctor_name(...) { ... };` and `let __instance: #class_ty = <#class_ty>::#ctor_name(...);` | `let __instance: #state_ty = match <#state_ty>::#ctor_name(...) { ... };` and `let __instance: #state_ty = <#state_ty>::#ctor_name(...);` | [emit-change: TWO sites in `gen_constructor_callback`, one per is_result branch — both use `#class_ty` literal in the `quote!`] |
| 7 | constructor `Default` fallback | 1705 | `<#class_ty as ::core::default::Default>::default()` and `let __instance: #class_ty` | `<#state_ty as ::core::default::Default>::default()` and `let __instance: #state_ty` | [emit-change: in `gen_default_constructor_callback`] |
| 8 | `Box::new(__instance)` + `Box::into_raw` | 1722-1748 (entire helper) | `gen_box_and_install_finalizer(class_ty)`; emits `Box::new(__instance)` (type inferred from let above) + `*mut #class_ty` cast in finalizer | `gen_box_and_install_finalizer(state_ty)`; emits `Box::new(__instance)` (type inferred — `__instance: #state_ty` from row 6/7) + `*mut #state_ty` cast in finalizer | [emit-change: helper takes `state_ty` arg; the `*mut #class_ty` interpolation in the finalizer at line 1739 changes to `*mut #state_ty`] |
| 9 | finalizer's drop type | 1739 | `drop(Box::from_raw(__raw_addr as *mut #class_ty));` | `drop(Box::from_raw(__raw_addr as *mut #state_ty));` | [emit-change: explicit cast in `gen_box_and_install_finalizer`] |
| 10 | method-callback receiver cast | 1156 | `unsafe { &mut *(__ext.value() as *mut #class_ty) }` | `unsafe { &mut *(__ext.value() as *mut #state_ty) }` | [emit-change: `gen_method_callback`] |
| 11 | method-callback user dispatch | 1098 | `<#class_ty>::#method_name(#receiver_ref, #(#call_args),*)` | `<#state_ty>::#method_name(#receiver_ref, #(#call_args),*)` | [emit-change] |
| 12 | setter-callback receiver cast | 1549 | `unsafe { &mut *(__ext.value() as *mut #class_ty) }` | `unsafe { &mut *(__ext.value() as *mut #state_ty) }` | [emit-change: `gen_setter_callback`] |
| 13 | setter-callback user dispatch | 1554 | `let _ = <#class_ty>::#method_name(#receiver_ref, #(#call_args),*);` | `let _ = <#state_ty>::#method_name(#receiver_ref, #(#call_args),*);` | [emit-change] |
| 14 | same-object getter cast | 1297 | `unsafe { &mut *(__ext.value() as *mut #class_ty) }` | `unsafe { &mut *(__ext.value() as *mut #state_ty) }` | [emit-change: `gen_same_object_getter_callback`] |
| 15 | same-object getter user dispatch | 1305 | `<#class_ty>::#method_name(#receiver_ref, #(#call_args),*)` | `<#state_ty>::#method_name(#receiver_ref, #(#call_args),*)` | [emit-change] |
| 16 | same-object Private symbol name | 1237 | `format!("__zs_same_object_{}_{}", class_ty, method_name)` | `format!("__zs_same_object_{}::{}_{}", module_path!(), marker_ty, method_name)` (qualified — see §2.7) | [emit-change + qualification: addresses critic round-1 C2(b)] |
| 17 | async-method receiver cast | 1472 | `let __instance: &#class_ty = unsafe { &*(__raw_addr as *mut #class_ty) };` | `let __instance: &#state_ty = unsafe { &*(__raw_addr as *mut #state_ty) };` | [emit-change: `gen_async_method_callback`, TWO interpolations in one let] |
| 18 | async-method user dispatch | 1473 | `<#class_ty>::#method_name(__instance, #(#call_args),*).await` | `<#state_ty>::#method_name(__instance, #(#call_args),*).await` | [emit-change] |
| 19 | reentry-guard error message | 1033-1034 | `format!("re-entered method `{}::{}` ...", class_ty, method_name)` | `format!("re-entered method `{}::{}` ...", marker_ty, method_name)` | [emit-change: `gen_reentry_guard` — better diagnostic] |
| 20 | must-new prologue message | 1609-1610 | `format!("Failed to construct '{class_name_str}': ...")` where `class_name_str = class_ty.to_string()` | same `format!`; `class_name_str` is now `marker_ty.to_string()` (changed via row 5) | [invariant: row 5 propagates] |
| 21 | Symbol.toStringTag default | 932-940 + 790-793 | inline `set_with_attr` using `to_string_tag_str = class_name_str` (where `class_name_str` is computed at line 689 from `class_ty.to_string()`) | same inline; `class_name_str` uses `marker_ty` via row 5 | [invariant: row 5 propagates; `#[v8_to_string_tag = "..."]` override unchanged] |
| 22 | iterable companion install | 446-456 | `quote! { <#class_ty>::__zs_install_iterable_methods(scope, __proto); }` | `quote! { <#marker_ty>::__zs_install_iterable_methods(scope, __proto); }` (companion is keyed off marker; see §2.9) | [emit-change] |
| 23 | constructor callback ident | 690 | `format_ident!("__{}_constructor_callback", class_ty)` | `format_ident!("__{}_constructor_callback", marker_ty)` | [emit-change: keep callback name JS-class-keyed for the install-fn ref at line 901] |
| 24 | method-callback ident | 970-972 | `format_ident!("__{}_{}_callback", class_ty, method)` | `format_ident!("__{}_{}_callback", marker_ty, method)` | [emit-change: marker-keyed because the install fn at impl block on marker references these] |

**Summary by category:**
- *State-keyed* (cast + dispatch): rows 6-15, 17-18 — 11 sites in
  the macro source.
- *Marker-keyed* (JS identity, install slots, error messages): rows
  1-5, 16, 19-23 — 12 sites.
- *Invariant under propagation* (a let derived from a renamed local):
  rows 20-21 — 2 sites.

**Implementation note (added in round 1).** Rows 6, 7, 8, 17 each have
the type ident appearing TWICE in a single `quote!` block (e.g. row 6
emits `let __instance: #class_ty = match <#class_ty>::#ctor_name(...)` —
two `#class_ty` interpolations). All instances within a single block
must consistently use either `#state_ty` or `#marker_ty` (state for
rows 6, 7, 8, 17; marker for the rest).

<!-- Revised in round 1 (addressing MAJOR #5): the constraint on
     generic state types is now explicit — same as today's bare-ident
     constraint on the marker. -->

### 4.2 Where the substitution is implemented inside `expand`

The cleanest implementation lives at the entry point. After
`extract_class_ident(&input.self_ty)` (`v8_class.rs:299-309`), we
introduce two locals (instead of one):

<!-- Revised in round 6 (addressing MINOR #17): the resolution
     logic is factored into a helper for readability. The early-
     return-with-compile-error pattern remains, just consolidated. -->

```rust
// Today's `extract_class_ident` is unchanged: requires
// `Type::Path(p) => p.path.get_ident()`, which only succeeds for
// a bare identifier with no generics.
let receiver_ty: &syn::Ident = match extract_class_ident(&input.self_ty) {
    Some(t) => t,
    None => return syn::Error::new_spanned(
        &input.self_ty,
        "#[v8_class] requires a plain type, e.g. `impl Headers`",
    ).to_compile_error().into(),
};

// New helper. Returns Some(path-to-marker) when the attribute is
// present, None otherwise.
let marker_path: Option<syn::Path> = extract_state_marker(&input.attrs);

// Resolve into a (state, marker) pair. Helper handles three error
// branches: (a) marker has generics / paths, (b) marker == receiver
// (the §4.7 hard error), (c) attribute absent (returns receiver,
// receiver — the no-op path).
let (state_ty, marker_ty): (&syn::Ident, syn::Ident) =
    match resolve_state_and_marker(receiver_ty, marker_path.as_ref()) {
        Ok(pair) => pair,
        Err(ts) => return ts.into(),  // already a compile-error TokenStream
    };

// Helper sketch:
fn resolve_state_and_marker<'a>(
    receiver_ty: &'a syn::Ident,
    marker: Option<&syn::Path>,
) -> Result<(&'a syn::Ident, syn::Ident), proc_macro2::TokenStream> {
    let Some(path) = marker else {
        // No-attr path: state == marker == receiver.
        return Ok((receiver_ty, receiver_ty.clone()));
    };
    let m_ident = path.get_ident().cloned().ok_or_else(|| {
        syn::Error::new_spanned(
            path,
            "#[v8_state_marker]: marker must be a bare type identifier \
             (no generics, no paths)",
        ).to_compile_error()
    })?;
    if &m_ident == receiver_ty {
        return Err(syn::Error::new_spanned(
            path,
            "#[v8_state_marker]: marker type matches impl receiver — \
             remove the attribute (use #[v8_class] alone for the no-op path)",
        ).to_compile_error());
    }
    Ok((receiver_ty, m_ident))
}
```

**Constraint (added in round 1).** Both `state_ty` and `marker_ty`
MUST be bare identifiers — same constraint as today's `class_ty`
(v8_class.rs:642-647 hard-codes `path.get_ident()`). This rules out:
- `impl Foo<T> { ... }` (generic state) — already disallowed today
  for marker, now explicitly disallowed for state.
- `#[v8_state_marker(some::module::Foo)]` — marker must be a single
  ident in scope at the impl block's site. Use `super::` path in a
  `use` statement above the impl if cross-module access is needed.

The constraint is consistent with today's behavior. Future relaxation
(generic states or path-qualified markers) is a separable change with
no implication for this proposal.

**Threading `state_ty` and `marker_ty` to the codegen helpers:**
- `gen_install(marker_ty, ...)` — marker-keyed (rows 1-5, 22 of §4.1).
- `gen_constructor_callback(marker_ty, state_ty, ...)` — both: marker
  for the must-new prologue (row 20) and the callback ident (row 23);
  state for the box payload (rows 6, 8, 9).
- `gen_default_constructor_callback(marker_ty, state_ty)` — both:
  marker for the callback ident; state for the `Default::default()`
  call (row 7).
- `gen_box_and_install_finalizer(state_ty)` — state-keyed (row 9).
- `gen_method_callback(state_ty, marker_ty, m)`,
  `gen_setter_callback(state_ty, marker_ty, m)`,
  `gen_same_object_getter_callback(state_ty, marker_ty, m)`,
  `gen_async_method_callback(state_ty, marker_ty, m)` — state for
  cast + dispatch (rows 10-15, 17-18); marker for the brand-check fn
  ident reference (row 3) and the callback ident (row 24).
- `gen_reentry_guard(marker_ty, method_name, mut_recv)` — marker for
  the diagnostic (row 19).

The parser adds one helper `fn extract_state_marker(&[Attribute]) -> Option<syn::Path>`
shaped exactly like `extract_inherit_base` (`v8_class.rs:265-277`),
and `strip_marker_attrs` learns one more recognised name
(`v8_class.rs:650-674`). Estimate ~120 LOC of macro diff.

### 4.3 Lifetime of state vs. wrapper

The `wrapper_global` keepalive in `gen_async_method_callback`
(`v8_class.rs:1444-1465`) pins the V8 wrapper. The V8 finalizer (which
drops the box) is wired against the wrapper Object's GC. So as long
as the wrapper is reachable, the box is alive. The state type is
agnostic. The `&StateTy` re-acquired per poll is sound by the same
argument as today's `&Self`.

### 4.4 `parse_params_skipping_self` quirk

The helper `parse_params_skipping_self` (`v8_class.rs:1925-1941`)
ignores the receiver argument when collecting positional JS args. It
filters by `FnArg::Typed`, so `&self` / `&mut self` (which are
`FnArg::Receiver`) are dropped naturally. This is type-name-free —
unchanged after the substitution.

### 4.5 `is_pin_scope_ref` / `is_wrapper_local`

These helpers (`v8_class.rs:1879-1912`) walk the *parameter type* AST
looking for `PinScope` / `Local<Object>` segments. They never inspect
the receiver. Substitution is a no-op for them.

### 4.6 The `args.this()` wrapper-Local synthetic

`gen_param_extractions` (`v8_class.rs:1864-1869`) binds a synthetic
`v8::Local<v8::Object>` parameter to `args.this()`. The wrapper Object
is a JS-side identity; the state type is irrelevant. Same code, same
behaviour.

<!-- Revised in round 1: case (c) (marker == state) is now decisive. -->

### 4.7 Compile-fail diagnostics

The macro's error messages should mention the right type when a user
mis-shapes their impl. Concretely:
- If `#[v8_state_marker(...)]` is present but the impl block has no
  `#[v8_constructor]` and `Self: Default` is the bound that fails,
  the error today is "the trait bound `RequestState: Default` is not
  satisfied" — **good**, that's the right type.
- If the user mistakenly writes `#[v8_state_marker(NotARealType)]`,
  the path resolution fails at the install codegen — `NotARealType` is
  used in the install slot, in `<NotARealType>::install`'s impl
  header. Compiler will say "cannot find type `NotARealType` in this
  scope" pointing at the install fn site.
- **If the marker is the same as the state** (e.g.,
  `#[v8_state_marker(RequestState)] impl RequestState`), the macro
  emits a hard error: `#[v8_state_marker]: marker type must differ
  from the impl receiver type — this case is the no-attribute path,
  remove the attribute`. Reasoning: this user intent is ambiguous
  (did the user mean to project, or did they accidentally double-name?)
  and silently treating it as no-op would mask typos. v1 takes the
  strict path. Implemented in `expand` after parsing both attrs:

  ```rust
  if let Some(path) = &marker_path {
      if let Some(m_ident) = path.get_ident() {
          if m_ident == receiver_ty {
              return syn::Error::new_spanned(path,
                  "#[v8_state_marker]: marker type matches impl receiver — \
                   remove the attribute (use #[v8_class] alone for the no-op path)"
              ).to_compile_error().into();
          }
      }
  }
  ```

### 4.8 Public visibility

`gen_install` emits `pub fn install(...)`. With Option B the install
fn lives on `impl <MarkerTy>` — so `Request::install` is the public
entry. Already what `core/init.rs` (and equivalent) call today.

---

## 5. Risk & back-compat

<!-- Revised in round 1 (addressing CRITICAL #2): the no-op claim is
     now a positive demonstration, with three concrete consumers walked
     row-by-row. Snapshot tests (insta) lock the codegen byte-for-byte. -->

### 5.1 No-attribute path is a strict no-op

**Claim:** when `#[v8_state_marker]` is absent, the macro emits
byte-identical code to today.

**Verification (positive demonstration over three concrete consumers):**

| Consumer | File | Has user `#[v8_constructor]` | `#[v8_inherit]` | `#[v8_iterable]` | `same_object` getters | `&mut self` methods |
|---|---|---|---|---|---|---|
| `Headers` | `web/headers.rs` | Yes | No | Yes (key/value) | No (today) | Yes (`set`, `delete`, `append`) |
| `AbortSignal` | `web/dom/abort_signal.rs` | Yes (default-shape) | Yes (`super::event_target::EventTarget`) | No | No (today) | No |
| `Blob` | `web/blob/blob.rs` | Yes | No | No | No | No |

For each, walk every substitution-table row (§4.1) under the no-attr
default `state_ty := class_ty := <ConsumerName>` AND
`marker_ty := class_ty := <ConsumerName>`:

- Rows 1-5: emit `__BrandSlot_<ConsumerName>`,
  `__InstallSlot_<ConsumerName>`, `__brand_check_<ConsumerName>`,
  `impl <ConsumerName> { fn install(...) }`, and
  `set_class_name("<ConsumerName>")`. **Identical to today**, because
  today the macro reads `class_ty = ConsumerName` and emits exactly
  these token strings.

- Rows 6-9 (constructor + box + finalizer): with `state_ty := class_ty`,
  the macro emits `let __instance: ConsumerName`,
  `<ConsumerName>::new(...)`, `Box::new(__instance)`, and `Box::from_raw(...
  as *mut ConsumerName)`. **Identical to today**.

- Rows 10-15, 17-18 (cast + dispatch): with `state_ty := class_ty`, every
  cast is `*mut ConsumerName` and every dispatch is
  `<ConsumerName>::method`. **Identical to today**.

- Row 16 (Same-object Private symbol): the qualification adds
  `module_path!()` (§2.7). Since no existing class uses
  `#[v8_getter(same_object)]` in production, this row's no-op claim
  is vacuous — the change is observable only the first time a
  consumer adopts `same_object`. The smoke test
  `crates/runtime/tests/v8_same_object_smoke.rs` exercises
  `same_object` and would observe the new qualified name on the
  Private; we update the test's assertion to match the qualified form
  (or, equivalently, observe behavior — `===` identity across reads —
  rather than the symbol name string).

  Net: row 16 is the ONE row where the no-attr path's emitted token
  string changes from today. We accept this because the symbol name
  is V8-internal (Private symbols are not reflectable from JS) and
  not API surface. The smoke test must be updated alongside the
  macro change. **This is the only behavioral observable change to
  any existing class under the no-attr path.**

- Rows 19, 22, 23, 24 (idents, error messages): use `marker_ty :=
  class_ty := ConsumerName`. **Identical to today**.

**Snapshot test (added in round 1, addresses MAJOR #3).** We add
`insta` to `crates/runtime-macros/Cargo.toml` and snapshot the
post-expansion token tree for one reference impl per attribute
combination:

| Snapshot | Reference impl |
|---|---|
| `class_basic.snap` | `#[v8_class] impl Foo { #[v8_constructor] fn new() -> Self ... }` (locks the no-attr emission baseline) |
| `class_with_state.snap` | `#[v8_class] #[v8_state_marker(Foo)] impl FooState { #[v8_constructor] fn new() -> Self ... }` |
| `class_with_inherit.snap` | `#[v8_class] #[v8_inherit(Bar)] impl Foo { ... }` (locks the inherit path under no-attr) |
| `class_with_state_inherit.snap` | `#[v8_class] #[v8_inherit(Bar)] #[v8_state_marker(Foo)] impl FooState { ... }` (locks the combination) |
| `class_with_iterable.snap` | `#[v8_class] #[v8_iterable(key=K, value=V)] impl Foo { ... }` |
| `class_with_async.snap` | `#[v8_class] impl Foo { #[v8_async_method] async fn fetch(&self) -> Result<...> { ... } }` |
| `class_with_same_object.snap` | `#[v8_class] impl Foo { #[v8_getter(same_object)] fn cached(&self, scope) -> Global<Object> { ... } }` |

The CI guard rejects any diff against the no-attr snapshots
(`class_basic`, `class_with_inherit`, `class_with_iterable`,
`class_with_async`) UNLESS the reviewer explicitly bumps them via
`cargo insta accept`. This locks the no-attr path against accidental
drift while letting the new shapes evolve.

**Behavioral verification.** Beyond snapshot equivalence, every
existing smoke test in `crates/runtime/tests/v8_*_smoke.rs` MUST
continue passing without modification (except `v8_same_object_smoke.rs`
to match the qualified Private name — see row 16 above). Lockdown is
the macro PR's CI gate.

### 5.2 `#[v8_inherit]` / `#[v8_inherit_intrinsic]` interaction

Three cases:

1. **Subclass uses `#[v8_state_marker]`, parent does not.** Subclass
   stores `Box<SubState>` in internal field 0; parent's install fn
   (called from the subclass's `inherit`) only operates on the
   FunctionTemplate, which is type-agnostic. Brand check on the
   subclass walks the prototype chain to the subclass's prototype OR
   the parent's prototype — either way, both classes' prototypes are
   reachable. ✓

2. **Subclass does not use `#[v8_state_marker]`, parent does.** This
   shape is theoretically odd — a subclass would typically want to
   embed the parent's state inside its own state. **Settled in
   round 1 (§2.10):** v1 supports any combination of (parent: attr,
   no-attr) × (child: attr, no-attr). Each class stores its OWN box
   (V8 instances carry one box per instance, but the child's
   `#[repr(C)]` first-field-of-parent layout means parent getter
   casts continue to work). See the worked example "CloseEvent :
   Event" in §2.10 for the four-quadrant table.

3. **Both use `#[v8_state_marker]`.** Same answer as #2 — supported,
   subject to the `#[repr(C)]` first-field convention. See §2.10.

> Net: state-projection is orthogonal to inheritance. The macro's
> doc comment documents that combining `#[v8_inherit]` with
> `#[v8_state_marker]` is fully supported (any combination), with
> the user maintaining `#[repr(C)]` layout for child states whose
> parent's getters cast to the parent's state type.

### 5.3 `#[v8_async_method]` borrow-safety after substitution

The doc comment in `gen_async_method_callback` (`v8_class.rs:1364-1378`)
explains why `*mut Self` re-acquired per poll is sound:

1. `wrapper_global` in the future capture pins the JS wrapper,
2. the macro rejects `&mut self` async methods, so only `&Self` is
   ever produced,
3. all future captures are owned (`Vec<u8>`, `String`, scalar), never
   borrowed.

After substitution: replace "Self" with "StateTy" in the argument.
- (1) still true — wrapper pins the *Object*, and the V8 finalizer is
  wired against the wrapper, dropping `Box<StateTy>` regardless.
- (2) still rejected — the macro's compile-time guard at
  `v8_class.rs:328-337` operates on the receiver mutability bit
  (`mut_recv`); doesn't read the type. Substitution preserves the
  guard.
- (3) unchanged — the user's method body only sees owned captures.

### 5.4 `#[SameObject]` cache key

The Private symbol name (`v8_class.rs:1238`) is a string. With Option
B's recommendation that we name it after the marker:
`__zs_same_object_<MarkerTy>_<method>`. No conflict possible with
existing classes; user code never observes the name.

### 5.5 Reentrancy guard set

Keyed by `External`'s pointer value (`v8_class.rs:1041`). Pointer
identity is preserved under our substitution (we still box once and
store the pointer once). Per-method thread-local — unchanged.

### 5.6 Public re-export of types

Each `#[v8_class]` consumer today exports the class type. Request's
existing module exports `Request`, `RequestState` (and helpers like
`RequestTemplateSlot`); after migration the surface shrinks (no
`request_constructor_callback` or `state_ptr` to publish), but the
public types stay the same. **No back-compat risk for downstream
crates** — the module's `pub use` surface narrows, and the macro
emits all the public callbacks under `pub(crate)`-scoped names already.

### 5.7 What changes for consumers NOT migrated to `#[v8_state_marker]`

Nothing. The macro's default behavior is unchanged. The only difference
is the parser learns a new attribute name, which is a no-op for impl
blocks that don't use it.

### 5.8 Risks not covered by §5.1–5.7

1. **`build_kernel_request` / `build_kernel_response` are NOT macro
   consumers.** They construct the Request / Response wrappers directly
   without going through `request_constructor_callback`. Migration must
   continue to expose the underlying machinery (template slot, state
   type, finalizer wiring) so the kernel fast paths still work. The
   migration plan (§7.4) reserves a `build_kernel_*` shape that holds
   `RequestState`/`ResponseState` and reuses the macro-installed
   FunctionTemplate. No macro change needed here, but the consumer
   migration must verify the template-slot integration.

2. **`try_native_response_body` / `is_native_response` /
   `try_native_response_websocket`** (`response.rs:171-219`). These
   helpers `state_ptr`-cast the response's internal field and inspect
   the state. **Settled in round 1 (§7.3.2):** the macro does NOT
   auto-emit a state-pointer accessor. The existing private
   `fn state_ptr(scope, obj) -> Option<*mut ResponseState>`
   (response.rs:126-135) stays as-is, used only by the three helpers
   above. Migration is no-op for these.

3. **Re-entrancy across `RefCell` borrows in user code.** `RequestState`
   is permeated with `RefCell<...>`. Today's hand-roll naturally avoids
   borrow conflicts because the constructor uses `&mut state:
   RequestState` directly, with no aliasing. After migration through
   the macro, getter callbacks use `&self` (i.e., `&RequestState`); we
   borrow `RefCell` cells one at a time. The behavior is the same, but
   we should add a smoke test that exercises the
   `request.headers.set(...) → fires synchronous callback → reads
   request.url` chain to ensure no aliased mutable borrow lurks.

---

## 6. Testing strategy

### 6.1 New test file: `crates/runtime/tests/v8_state_smoke.rs`

Coverage matrix:

| Test | What it proves |
|---|---|
| `state_basic_roundtrip` | `#[v8_state_marker(M)] impl S` with one `#[v8_constructor]` returning `S`, one getter, one method. JS observes the marker name (`new Request(...) instanceof Request`). |
| `state_default_ctor` | No user constructor; `S: Default` is enough; `new M()` allocates a default `S`. Compile-fail mirror in `tests/ui/`: `S` without `Default` impl fails with the right diagnostic. |
| `state_marker_no_attribute` | A class that does NOT use `#[v8_state_marker]` continues to work exactly as today. (Token-tree equivalence is the implementation-side proof; this is the behavioral mirror.) |
| `state_with_inherit` | `#[v8_state_marker(M)]` + `#[v8_inherit(Parent)]`. Three concrete assertions: (a) `M.prototype.<own_method>` returns the right value (own state read works); (b) `Parent.prototype.<parent_method>.call(m_instance)` returns the right value (parent-state read via the `#[repr(C)]` first-field works); (c) `m_instance instanceof Parent === true` (brand-check via parent's prototype walk). The reference impl uses a `#[repr(C)]` ChildState with `parent: ParentState` first field, mirroring CloseEventState. |
| `state_with_async_method` | `#[v8_async_method] async fn foo(&self) -> ...` re-acquires `&StateTy` per poll. Returns Promise, settles correctly under microtask + sleep. |
| `state_with_iterable` | `#[v8_state_marker]` is silently ignored on the iterator companion class (§2.9). Iterator state is its own struct; the iterable parent's state is `StateTy`. |
| `state_brand_check_cross_class` | Two distinct `#[v8_state_marker]` classes A and B, with possibly identical state types (e.g., both wrap `Cell<u32>`). Calling `A.prototype.foo.call(b_instance)` throws `Illegal invocation` per WebIDL §3.7. |
| `state_reentrancy_guard` | `#[v8_state_marker]` + `&mut self` setter that fires a callback re-entering the same instance. Throws TypeError before the unsafe deref. |
| `state_finalizer_drops_state_type` | Holds an `Arc<AtomicUsize>` in `StateTy::Drop` impl; verify count goes to 0 after V8 GC. (Mirror of `gc_finalizer` in `v8_class_smoke.rs`.) |
| `state_same_object_getter` | `#[v8_getter(same_object)]` returning `Global<Object>` from a `&StateTy` receiver. Two reads return `===` identical Objects. |

<!-- Revised in round 2 (addressing MAJOR #8): added the marker == state
     hard error from §4.7 to the compile-fail matrix. -->

### 6.2 Compile-fail tests (`crates/runtime-macros/tests/ui/`)

Today the `runtime-macros` crate has no UI test scaffold; the runtime
crate has compile-fail mirrors in `crates/runtime/tests/v8_async_method_smoke.rs`'s
inline `#[allow(dead_code)]` patterns. We add explicit
`compile-fail`-shaped coverage:

| Compile-fail | Diagnostic |
|---|---|
| `#[v8_state_marker(NonExistent)]` | `cannot find type 'NonExistent' in this scope` (resolved at the install fn) |
| `#[v8_state_marker(M)]` + ctor returns `Self` of marker type | type mismatch (ctor returns `M`, macro casts to `MState`); expect a clear error |
| `#[v8_state_marker(M)] #[v8_state_marker(N)]` (duplicate) | `#[v8_class]: duplicate #[v8_state_marker]` (macro-emitted) |
| `#[v8_state_marker(M)] impl S` where `S: !Default` and no user ctor | `the trait bound 'S: Default' is not satisfied` |
| `#[v8_state_marker(Foo)] impl Foo` (marker == state) | `#[v8_state_marker]: marker type matches impl receiver — remove the attribute (use #[v8_class] alone for the no-op path)` (macro-emitted, per §4.7) |
| `#[v8_state_marker(some::path::Foo)] impl Bar` (path-qualified marker) | `#[v8_state_marker]: marker must be a bare type identifier (no generics, no paths)` (macro-emitted, per §4.2) |

<!-- Revised in round 1 (addressing MAJOR #6): the v1 referenced
     test files that don't exist (wpt_fetch.rs / wpt_response.rs).
     Verified actual test names by listing crates/runtime/tests/. -->

### 6.3 WPT regression coverage

Once Request/Response migrate (§7), we run the existing WPT subset
(`crates/runtime/tests/wpt_*.rs`):

| WPT file | Today | After migration |
|---|---|---|
| `wpt_fetch_request.rs` | Request constructor + getters + clone() | Pass rate unchanged |
| `wpt_fetch_response.rs` | Response constructor + getters + clone() + statics | Pass rate unchanged |
| `wpt_fetch_body.rs` | text / json / arrayBuffer / bytes / blob / formData on both Request and Response | Pass rate unchanged (Body trait dispatch is unchanged) |
| `wpt_fetch_basic.rs` | High-level fetch flow, exercises both classes | Pass rate unchanged |
| `wpt_fetch_basic_network.rs` | Network-side flow, kernel fast path | Pass rate unchanged (build_kernel_request slot rename is the only delta) |
| `wpt_fetch_redirect.rs` | Response.redirect static + kernel redirect handling | Pass rate unchanged |
| `wpt_fetch_abort.rs` | AbortSignal/Request.signal interaction | Pass rate unchanged ([SameObject] cache change is internal) |

Plus the in-tree integration tests:

| File | What it covers |
|---|---|
| `fetch_request.rs` | Direct Request unit tests (constructor variants, edge cases) |
| `fetch_response.rs` | Direct Response unit tests + statics |
| `fetch_body.rs`, `fetch_body_blob.rs` | Body materialization |
| `fetch_native.rs`, `fetch_native_install.rs` | Native-side install + kernel-fast-path |
| `call_fetch_handler.rs` | The `runtime.rs::call_fetch_handler` dispatch path |

Spec deviations in the hand-roll (e.g., the `duplex: "full"` rejection
at `request.rs:467-476`, the 101-status carve-out at
`response.rs:444-464`) are preserved by writing the same logic in the
migrated `#[v8_constructor]`.

**CI gate for the migration PR:** zero net WPT pass-rate regression
across all `wpt_fetch_*.rs` and `wpt_headers.rs`. A single
test-by-test allowlist of expected diffs is acceptable IF documented
in the PR description (e.g., test that depends on the V8 Private
symbol name string would fail; we update the test). No silent
drops.

<!-- Revised in round 5 (addressing MINOR #15): the snapshot story
     is committed in §5.1; this section now cross-refs and adds the
     CI-gate detail. -->

### 6.4 Snapshot tests for codegen output

Committed in §5.1's "Snapshot test (added in round 1, addresses
MAJOR #3)" — `insta` is added to `crates/runtime-macros/Cargo.toml`
and seven reference shapes are snapshotted. The Phase 1 PR's CI gate
rejects any unauthorized diff against the seven snapshot files;
intentional bumps require `cargo insta accept` with reviewer audit.

The snapshot stability concerns (rustc / quote-crate version pinning)
are documented in `crates/runtime-macros/README.md` per §8 settled-
question 9.

---

## 7. Migration path

### 7.1 Phase 1 — land the macro change in isolation

PR scope:
- Add `#[v8_state_marker(...)]` parsing.
- Wire `state_ty` / `marker_ty` through the codegen (§4.1).
- Add `crates/runtime/tests/v8_state_smoke.rs` with the §6.1 matrix.
- Compile-fail tests (§6.2).
- Update `crates/runtime-macros/lib.rs` doc comment.

Reviewers focus on the substitution-table diff. No consumer is
migrated yet, so all 30+ existing `#[v8_class]` users continue to work
unchanged. Land this PR alone.

<!-- Revised in round 1 (addressing CRITICAL #3, MAJOR #4, MAJOR #7,
     Missing Concept #1, #5): the migration sketch now spells out
     PENDING_CT, [SameObject] interaction, BodyMarker / install_body_methods,
     RequestTemplateSlot disposition, and the kernel fast-path
     interaction in concrete terms. -->

### 7.2 Phase 2 — Request migration

`crates/runtime/src/web/fetch/request.rs`. Diff laid out in 6 chunks:

<!-- Revised in round 2 (addressing MAJOR #11): state_ptr stays
     private; addressing CRITICAL #6: body / bodyUsed getters are
     installed by install_body_methods, NOT by the macro. -->

#### 7.2.1 What stays verbatim (no LOC change)

- `pub struct Request;` — the marker unit struct.
- `pub struct RequestState { ... }` — the boxed state. Field types
  are unchanged.
- `impl Default for RequestState` — the spec defaults (request.rs:84-107).
- `impl BodyMarker for Request` — `CLASS_LABEL = "Request"` (request.rs:118-120).
- `impl Body for Request` — `body_state` and `content_type` projections
  (request.rs:122-152). The trait dispatch reads internal field 0 and
  casts to `*mut RequestState`, which is what the macro now boxes.
  Sound by construction (the macro's box payload type matches the
  `Body` impl's cast).
- `state_ptr(scope, obj) -> Option<*mut RequestState>` (request.rs:170-179).
  Stays **private** (no `pub` / `pub(crate)` change — it's used only
  inside the same module by the `Body` trait impl). The macro does
  NOT auto-emit this (Open Question 3 settled in §8). Each consumer
  that needs raw-pointer access defines its own.

**About the `body` and `bodyUsed` getters:** these are NOT installed
by the macro — they come from the `Body` trait via
`install_body_methods::<Request>(scope, proto)` (request.rs:237).
Specifically (verified by reading
`crates/runtime/src/web/fetch/body/consumers.rs:53-100`):
- `install_body_methods` installs **8 prototype properties**:
  - `body` (getter, returns `ReadableStream | null`),
  - `bodyUsed` (getter, returns `bool`),
  - `text`, `json`, `arrayBuffer`, `bytes`, `blob`, `formData`
    (methods, return Promise<...>).
- All 8 read internal field 0 via the `Body` trait's `body_state(scope, this)`
  projection.
- Migration is no-op: `install_body_methods::<Request>` is called
  EXACTLY ONCE after `Request::install` returns, from
  `setup_globals`. The trait reads through internal field 0 to
  `*mut RequestState` — same cast type as the macro's emitted
  callbacks. Zero conflict.

So the migrated `#[v8_class] impl RequestState` block defines:
- 1 `#[v8_constructor]`
- 15 `#[v8_getter]` for the simple state attributes (method, url,
  destination, referrer, referrerPolicy, mode, credentials, cache,
  redirect, integrity, keepalive, isReloadNavigation,
  isHistoryNavigation, duplex, priority)
- 2 `#[v8_getter(same_object)]` for headers + signal
- 1 `#[v8_method]` for clone

Total **19 macro-installed prototype properties + 8 trait-installed
prototype properties** = 27, matching today's hand-roll surface.
<!-- Revised in round 2 (addressing CRITICAL #8, MAJOR #9):
     fallback for RequestPrototypeSlot is now spelled out for v1, not
     deferred to MAC-02. -->

- `build_kernel_request` (request.rs:780-846). The body is unchanged
  in spirit — manual `RequestState`, manual `Box::new`, manual
  finalizer. **Source changes:**

  1. Replace `scope.get_slot::<RequestTemplateSlot>()` with
     `scope.get_slot::<__InstallSlot_Request>()` (the macro's install
     slot — caches the FunctionTemplate Global). The
     `RequestTemplateSlot::class_tmpl` field is now redundant.
  2. Add a v1-defined `RequestPrototypeSlot` to cache the prototype
     Object — populated on first `build_kernel_request` call (or, when
     MAC-02 lands, via the post-install hook). v1 source:

     ```rust
     pub(crate) struct RequestPrototypeSlot(pub v8::Global<v8::Object>);

     pub fn build_kernel_request(scope, ...) -> Option<v8::Local<v8::Object>> {
         // 1. FunctionTemplate from macro slot.
         let tmpl_global = scope.get_slot::<__InstallSlot_Request>()?.0.clone();
         let tmpl = v8::Local::new(scope, tmpl_global);

         // 2. Prototype: cached on first call.
         let proto_global = match scope.get_slot::<RequestPrototypeSlot>() {
             Some(s) => s.0.clone(),
             None => {
                 let func = tmpl.get_function(scope)?;
                 let key = v8::String::new(scope, "prototype")?;
                 let proto: v8::Local<v8::Object> = func.get(scope, key.into())?.try_into().ok()?;
                 let g = v8::Global::new(scope, proto);
                 scope.set_slot(RequestPrototypeSlot(g.clone()));
                 g
             }
         };
         let proto = v8::Local::new(scope, proto_global);

         // 3-5. Allocate via instance template, set proto, populate state, install finalizer.
         // (Same body as today, ~50 LOC.)
         ...
     }
     ```

  The 3-V8-call prototype lookup runs ONCE per isolate (steady-state
  the slot is populated). Hot-path cost: one slot read per kernel-fast-
  path dispatch (~5ns).

  *Concrete migration delta in `build_kernel_request`*: ~-3 LOC
  (remove `RequestTemplateSlot.class_tmpl` field, replace with
  `__InstallSlot_Request` lookup); LOC neutral overall.

  *Perf gate (added in round 2 addressing CRITICAL #8).* The estimated
  ~100ns cost cited in v1 was an unverified guess. **The proper gate is
  end-to-end:** run `crates/runtime/benches/run_zerobench.sh` (the
  existing HTTP/SSE/WS benchmark suite) on the macro PR's pre-
  substitution baseline AND post-migration commit. Compare req/s on
  the `fetch-echo` and `static` scenarios. Commit-by-commit benchmark
  gate runs as part of Phase 2 PR's CI, NOT deferred. Acceptable
  regression: < 5% req/s, < 10% p99 latency, on the project's standard
  bench box. If exceeded, the PR adds the `RequestPrototypeSlot` plus
  any consequent micro-optimizations as part of the same PR — no
  separate "fix later" commit.

#### 7.2.2 What gets removed (deleted hand-roll)

| Hand-rolled item | Lines | LOC | Replaced by |
|---|---|---|---|
| `pub fn install_global` | 221-259 | -39 | `Request::install` (macro-emitted) called from `core/init.rs::setup_globals` |
| `fn install_method` (helper) | 261-271 | -11 | macro install codegen |
| `fn install_getter` (helper) | 273-289 | -17 | macro install codegen |
| `fn install_request_getters` | 291-319 | -29 | macro install codegen |
| `fn request_constructor_callback` | 332-741 | -410 | `#[v8_constructor] fn new(...) -> Result<Self, OpError>` body (the spec walk migrates verbatim) |
| Per-getter callbacks (method, url, destination, referrer, referrerPolicy, mode, credentials, cache, redirect, integrity, keepalive, isReloadNavigation, isHistoryNavigation, duplex, priority — 15 getters) | est. ~270 | -270 | one `#[v8_getter]` per attribute, one-line bodies (e.g. `fn method(&self) -> String { self.method.borrow().clone() }`) |
| `fn headers_getter`, `fn signal_getter` | est. ~80 | -80 | one `#[v8_getter(same_object)]` each |
| `fn request_clone_callback` | est. ~60 | -60 | one `#[v8_method] fn clone(&self, scope) -> v8::Local<v8::Object>` |
| `fn tee_stream` (helper) | est. ~25 | -25 | moves to a private helper inside the migrated `clone` body |
| `fn is_request_instance`, `fn is_readable_stream_global_instance` | est. ~30 | -30 | the macro's brand check replaces `is_request_instance`; readable-stream check stays as a helper used by the constructor body |
| **Subtotal removed** |  | **~-971** | |

#### 7.2.3 What gets added (new macro consumers)

| Added | Estimate (LOC) |
|---|---|
| `#[v8_class] #[v8_state_marker(Request)] impl RequestState { ... }` block header | +3 |
| `#[v8_constructor] fn new(...) -> Result<Self, OpError>` (spec walk verbatim from `request_constructor_callback`) | ~+410 (moved, not new) |
| 15 `#[v8_getter]` simple bodies | ~+45 (3 LOC each) |
| 2 `#[v8_getter(same_object)]` for headers + signal | ~+40 |
| 1 `#[v8_method] fn clone(...)` | ~+60 (moved) |
| 1 `tee_stream` private helper inside `impl RequestState` | ~+25 (moved) |
| **Subtotal added** | **~+583** |

#### 7.2.4 PENDING_CT thread-local migration (addresses MAJOR #4)

The hand-roll uses a thread-local HashMap (`request.rs:944-950`) keyed
by `state as *const RequestState as usize` to defer applying
Content-Type until headers are built. **This is a stack-pointer key:
the address of the not-yet-boxed RequestState during the constructor's
lifetime.**

Under the macro, the constructor returns `Result<RequestState,
OpError>` BY VALUE. The macro takes the value, boxes it, and installs
the pointer in internal field 0. The address of the value during
construction (a stack slot in the macro's `make_instance` block, line
1641) DIFFERS from the address of the box after `Box::into_raw`. The
thread-local key would not survive the move.

<!-- Revised in round 2 (addressing CRITICAL #7): the migration
     is verified to not break callers — there's exactly one call site
     today (request.rs:612), already taking `&mut state`. -->

**Migration approach.** Replace the thread-local with a local field on
the `RequestState` struct itself, populated within the constructor and
applied before the constructor returns:

```rust
pub struct RequestState {
    ...
    /// Pending Content-Type to apply to headers before this state is
    /// boxed into V8 internal field 0. Always None after the
    /// constructor returns; the constructor body applies it via
    /// `apply_pending_content_type` and then sets this back to None.
    pending_content_type: RefCell<Option<String>>,
    ...
}

impl RequestState { /* manual sibling impl, NOT inside the macro impl */
    fn ensure_content_type(&mut self, ct: String) {
        *self.pending_content_type.borrow_mut() = Some(ct);
    }
}
```

The constructor body, inside the macro impl, does:
```rust
#[v8_constructor]
fn new(...) -> Result<RequestState, OpError> {
    let mut state = RequestState::default();
    // ... spec walk that may call state.ensure_content_type(ct) ...
    let headers_obj = build_request_headers(scope, init_obj, ...)?;
    apply_pending_content_type(scope, &mut state, headers_obj);
    *state.headers.borrow_mut() = Some(v8::Global::new(scope, headers_obj));
    // ... rest of spec walk ...
    Ok(state)
}
```

**Caller-side verification (added in round 2).** Today there's
exactly ONE caller of `ensure_content_type` (verified via
`grep ensure_content_type request.rs`): line 612, inside
`request_constructor_callback`, with `&mut state` form. The
migration is mechanical: rewrite `ensure_content_type(scope, &mut
state, ct)` as `state.ensure_content_type(ct)`. The `scope` arg is
unused inside `ensure_content_type` today (line 953, `let _ = scope;`)
so it's safe to drop on the way to the method form.

Net: the thread-local goes away (-7 LOC), one new struct field
appears (+1 LOC), and the `ensure_content_type` /
`apply_pending_content_type` helpers shift from thread-local-keyed to
struct-field-keyed (~+4 LOC change). The PENDING_CT mechanism is no
longer "internal — hidden behind the macro" (the v1 framing was
wrong). It's a struct field, plain and visible.

#### 7.2.5 LOC delta summary

| Bucket | LOC |
|---|---|
| Removed hand-roll (§7.2.2) | -971 |
| Added macro consumer (§7.2.3) | +583 |
| PENDING_CT migration (§7.2.4) | -2 (net) |
| build_kernel_request slot rename (§7.2.1) | 0 |
| **Net** | **-390** |

The brief's target was -700 LOC for Request alone. **The actual delta
falls short by ~310 LOC** because the spec walk is large and migrates
verbatim into the constructor body. This is honest — the macro
removes wiring boilerplate, not the spec logic itself.

If the spec walk is decomposed into smaller helpers (e.g.,
`parse_input`, `apply_init_overrides`, `build_request_signal` —
already exists), some of those helpers can move OUT of the impl block
into the module's private fns (no LOC change, just relocation), making
the `#[v8_constructor]` body smaller. **The end-state Request module
is estimated at ~480 LOC**, down from today's 1341 LOC.

#### 7.2.6 RequestTemplateSlot disposition (addresses MAJOR #7)

`pub struct RequestTemplateSlot { class_tmpl, prototype }` (request.rs:213-216):

- `class_tmpl: v8::Global<v8::FunctionTemplate>` is now redundant with
  the macro's `__InstallSlot_Request.0` (same Global, same isolate).
  **Remove RequestTemplateSlot.class_tmpl.**
- `prototype: v8::Global<v8::Object>` has no equivalent in the macro's
  install slot. Two options:
  - (a) Define `pub(crate) struct RequestPrototypeSlot(v8::Global<v8::Object>)`
    in request.rs and stash it during a post-install hook (MAC-02).
  - (b) Resolve the prototype from the FunctionTemplate on demand in
    `build_kernel_request` (3 V8 calls). For v1 we go with (b) because
    MAC-02 is deferred; revisit when MAC-02 lands.

#### 7.2.7 Code sketch — final shape of migrated request.rs

```rust
pub struct Request;
impl BodyMarker for Request { const CLASS_LABEL: &'static str = "Request"; }
impl Body for Request { /* unchanged */ }

pub struct RequestState { ... }                     // unchanged fields + pending_content_type
impl Default for RequestState { ... }               // unchanged
impl RequestState {                                  // sibling impl for ensure_content_type
    fn ensure_content_type(&mut self, ct: String) { ... }
}

pub(crate) fn state_ptr(scope, obj) -> Option<*mut RequestState> { ... }

#[v8_class]
#[v8_state_marker(Request)]
impl RequestState {
    #[v8_constructor]
    fn new(input: v8::Local<v8::Value>, init: v8::Local<v8::Value>)
        -> Result<Self, OpError>
    { /* spec walk; uses self.ensure_content_type(...); applies pending CT before return */ }

    #[v8_getter] fn method(&self) -> String { self.method.borrow().clone() }
    #[v8_getter] fn url(&self) -> String { self.url.borrow().clone() }
    // ... 13 more simple getters ...

    #[v8_getter(same_object)]
    fn headers<'s>(&self, scope: &mut v8::PinScope<'s, '_>) -> v8::Global<v8::Object> {
        if let Some(g) = self.headers.borrow().as_ref() {
            return g.clone();
        }
        // Lazy mint for the kernel fast path that pre-populated state.headers as None.
        let h = crate::headers::build_empty(scope);
        v8::Global::new(scope, h)
    }

    #[v8_getter(same_object)]
    fn signal<'s>(&self, scope: &mut v8::PinScope<'s, '_>) -> v8::Global<v8::Object> { ... }

    #[v8_method]
    fn clone<'s>(&self, scope: &mut v8::PinScope<'s, '_>) -> v8::Local<'s, v8::Object> { ... }
}

// Body methods installed via the trait — UNCHANGED.
// Note: this happens OUTSIDE the macro impl; install_body_methods reads a
// FunctionTemplate's prototype and adds text/json/arrayBuffer/bytes/blob/formData.
// The macro's install fn has no knowledge of `Body`; we wire it manually after.

// Need a post-install hook for `install_body_methods`. Two options:
// (1) call `install_body_methods::<Request>(scope, proto)` once after
//     `Request::install(scope)` is first run (e.g., in setup_globals).
// (2) MAC-02: a `#[v8_post_install]` hook on the impl that the macro
//     calls after laying down the prototype methods.
// v1 takes (1): explicit call in setup_globals. MAC-02 lands later.

pub fn build_kernel_request(scope, ...) -> Option<v8::Local<v8::Object>> {
    // 3-V8-call prototype lookup via __InstallSlot_Request (no separate slot).
    ...
}
```

<!-- Revised in round 1 (addressing CRITICAL #4, MAJOR #7): the
     try_native_response_body story is settled (consumer-defined
     helper, NOT macro-emitted), and the static-method handling is
     spelled out. -->

### 7.3 Phase 3 — Response migration

`crates/runtime/src/web/fetch/response.rs`. Same shape as Request,
with three extra concerns:

#### 7.3.1 Static methods (Response.error / redirect / json)

`Response.error()`, `Response.redirect(url, status?)`, and
`Response.json(data, init?)` are static methods on the constructor
function (response.rs:264-266 install them via `install_static`).
**The macro does not yet support `#[v8_static_method]`** (MAC-07).
For v1 they remain hand-rolled.

This means the migrated `response.rs` retains:
- `static_error_callback` (response.rs:872-913),
- `static_redirect_callback` (response.rs:915-985),
- `static_json_callback` (response.rs:987-1086),
- `init_supplied_content_type` (response.rs:1091-1131) helper used
  by `static_json_callback`,
- A small post-install hook (called from `setup_globals` after
  `Response::install`) that walks
  `Response::install(scope).get_function(scope)?` and attaches the
  three static methods. Estimated +25 LOC for the post-install hook,
  net of the loss of `install_static` helper.

When MAC-07 lands, these become `#[v8_static_method]` annotations
inside the macro impl block — purely additive.

#### 7.3.2 try_native_response_body / try_native_response_websocket / is_native_response (addresses CRITICAL #4)

`response.rs:171-220` defines three public helpers that read
`*mut ResponseState` from internal field 0:

- `is_native_response(scope, obj) -> bool`
- `try_native_response_body(scope, obj) -> Option<NativeResponseBody>`
- `try_native_response_websocket(scope, obj) -> Option<v8::Global<v8::Object>>`

These are consumed by `crate::http::inspect_response` and
`runtime.rs::call_fetch_handler` — the kernel-side path. They MUST
survive migration verbatim.

**Decision (settles old Open Question 3):** the macro does NOT
auto-emit a state-pointer accessor. Reasoning:

- Auto-emitting `pub fn state_ptr` exposes raw pointers in the public
  API surface. That contradicts the "narrow surface" principle and
  the `unsafe`-isolation policy in CLAUDE.md / AGENTS.md.
- The hand-defined `state_ptr` helper is 9 LOC; auto-emission saves
  9 LOC × N consumers = ~30 LOC across the whole codebase. Not
  worth the API-surface cost.
- Each consumer's helper can apply consumer-specific brand checks
  (e.g., Response's `is_native_response` short-circuits before
  brand-check, allowing duck-typed `{status, url}`-shaped handler
  returns to fall through gracefully). A macro-emitted helper would
  apply the marker-based brand check unconditionally.

So `state_ptr` stays defined in `response.rs` (the existing
`fn state_ptr(scope, obj) -> Option<*mut ResponseState>` at
response.rs:126-135) as `pub(crate)`, and `is_native_response` /
`try_native_response_body` / `try_native_response_websocket`
continue to use it. Migration is no-op for these three helpers.

The macro provides the building blocks they need:
- `__InstallSlot_Response` for any future template-slot lookups.
- `__brand_check_Response` (as a `pub(crate)` fn — see §4.1 row 3) if
  the consumer wants a strict brand check rather than the lax
  "is internal field 0 an External" today.

#### 7.3.3 The 101 / WebSocket carve-out (response.rs:444-464)

The hand-roll's status-range check allows `n == 101.0` for the
WebSocket upgrade extension. This carve-out is policy, not spec, and
is preserved verbatim in the migrated `#[v8_constructor]` body. Same
for the `webSocket` field in init being preserved on
`state.web_socket` (response.rs:496-506).

#### 7.3.4 LOC delta summary

| Bucket | LOC |
|---|---|
| Removed hand-roll (constructor + 8 getters + clone + install_global, install_method, install_getter, install_static, install_response_getters) | est. -650 |
| Added macro consumer (constructor body + 8 `#[v8_getter]` simple bodies + 1 `#[v8_getter(same_object)]` for headers + 1 for webSocket + clone) | est. +480 |
| Static methods + post-install hook | +25 |
| **Net** | **est. -145** |

End-state response.rs estimated at ~985 LOC, down from 1131.
Combined with Request (-390 / -393 from §7.2): **net -535 LOC**
across both files.

The brief's combined target was -1200 LOC. Honest delta is half of
that. The remaining ~600 LOC is in the spec walks (Request and
Response constructors, ~410 + ~150 = ~560 LOC of spec logic that
can't be macro-eliminated). This is the floor.

### 7.4 Spec deviations to preserve

Each migration must carry over each deviation the hand-roll has from
the spec. Inventory:

**Request hand-roll (from `request.rs`):**
- `DEFAULT_BASE_URL = "http://localhost/"` (synthetic API base URL,
  `request.rs:45`). Stays in the migrated constructor body.
- `duplex: "full"` rejected with TypeError (`request.rs:467-476`). Stays.
- Forbidden methods CONNECT/TRACE/TRACK and method-token validation
  (`request.rs:920-944`). Stays.
- Disturbed-input check ordering vs. validation throws (`request.rs:496-565`).
  Stays — preserves WPT pass.
- Pending-Content-Type via `thread_local PENDING_CT` (`request.rs:950-1005`).
  Stays as-is — internal mechanism, hidden behind the macro.
- Lazy-mint `request.signal` on first read for the kernel fast path
  (`request.rs:1191-1216`). Implemented as a `#[v8_getter(same_object)]`
  with the user method handling the lazy-mint.
- Tee-on-construct semantics for stream input (`request.rs:656-675`).
  Stays in the migrated constructor.

**Response hand-roll (from `response.rs`):**
- Status range 200..=599 plus 101 carve-out for WebSocket upgrade
  (`response.rs:444-464`). Stays.
- `webSocket` extension preservation (`response.rs:496-506`,
  `try_native_response_websocket`). Stays.
- Null-body status set 101/103/204/205/304 (`response.rs:42-46`,
  `is_null_body_status`). Stays.
- Redirect status set 301/302/303/307/308 (`response.rs:48-50`). Stays.
- `is_valid_reason_phrase` (`response.rs:569-584`). Stays.
- `static_error_callback` immutability seal of headers (`response.rs:911-917`).
  Stays in the static method's body (still hand-rolled until MAC-07).
- `init_supplied_content_type` heuristics for Response.json
  (`response.rs:1097-1137`). Stays.

None of these require changes to the proposed macro — they all live in
the `#[v8_constructor]` body or in the static-method hand-rolls that
remain.

### 7.5 Phase 4 — follow-up items

Once Request and Response are migrated:
- MAC-08 (`#[v8_getter(same_object, project = field)]`): cache on a
  `RefCell<Option<Global>>` field of `StateTy` instead of a V8
  private symbol. Removes the `state.headers.borrow_mut() = ...` lazy
  init from getters; macro takes care of it.
- MAC-07 (`#[v8_static_method]`): migrate Response.error / .redirect /
  .json onto the macro.

These are independent and incremental.

### 7.6 Estimated ship time

Per `feedback_estimates_hours_not_weeks` (industry estimates / 40):
- Phase 1 (macro + smoke tests): ~2 hours.
- Phase 2 (Request migration): ~3 hours including WPT verification.
- Phase 3 (Response migration): ~2 hours.
- Total to land all three: ~7 hours of focused work.

---

<!-- Revised in round 1 (addressing MINOR #2): all answerable open
     questions now have a settled answer. Only one (E2E perf gate)
     remains, with a clear test plan. -->

## 8. Open questions

### Settled in round 1

1. **Attribute polarity.** ~~Option B as written above puts the
   user's `impl` on the state type and names the marker via
   `#[v8_state_marker(M)]`.~~ — **Settled: Option B (state-shaped
   impl, `#[v8_state_marker(Marker)]`).** Reason: matches existing
   codebase convention (CloseEventState, AbortSignal, MessageEventState
   are already `#[v8_class] impl <state-struct>`). See §2.2.

2. **Default-derived constructor with separate state.** ~~Acceptable
   to require `S: Default`?~~ — **Settled: yes, identical to today's
   `Self: Default` requirement.** Both `RequestState` and
   `ResponseState` already implement `Default`. We do NOT reject
   default-derived constructors when `state != marker`; that would
   over-constrain future classes that legitimately want a default-
   constructible state. v1 takes the permissive path. See §2.3.

3. **`state_ptr` accessor emission.** ~~Should the macro auto-emit?~~ —
   **Settled: NO.** The macro does not auto-emit a state-pointer
   accessor. Each consumer that needs raw-pointer access defines its
   own `pub(crate) fn state_ptr` (~9 LOC). Reasoning: API surface,
   `unsafe`-isolation, and consumer-specific brand-check policies
   (see §7.3.2).

4. **Inheritance + state projection.** ~~Does v1 support all
   combinations?~~ — **Settled: yes, all four (parent: attr,
   no-attr) × (child: attr, no-attr) combinations are supported.**
   The user must maintain the `#[repr(C)]` first-field convention
   for the child's state struct (existing convention). See §2.10.

5. **Reentrancy guard message.** ~~Marker or state?~~ — **Settled:
   marker.** The user thinks of the JS class identity; "Request" is
   more recognisable than "RequestState" in error output. Per row
   19 in §4.1.

6. **Same-object Private symbol naming.** ~~Marker name only, or
   qualified?~~ — **Settled: qualified by `module_path!()`.** v1's
   marker-only proposal failed cross-module collision. The qualified
   form `__zs_same_object_<module_path>::<MarkerTy>_<method>` is
   collision-free. See §2.7 and row 16 in §4.1.

7. **Macro test scaffold (insta snapshots).** ~~Worth establishing?~~ —
   **Settled: yes, add insta to `crates/runtime-macros/Cargo.toml`**
   and snapshot the seven reference shapes in §5.1. The byte-level
   regression guard is the one piece of evidence the no-attr back-
   compat claim isn't possible to fake.

### Still open (require landing PR's CI to settle empirically)

8. **build_kernel_request perf parity.** §7.2.6 punts on
   RequestPrototypeSlot until MAC-02 lands; v1 resolves the prototype
   from the FunctionTemplate on demand (3 V8 calls). The 3 V8 calls
   per kernel-fast-path Request build add ~100ns to `inspect_response`'s
   hot path. The Phase 2 PR must benchmark via
   `crates/runtime/benches/fetch_kernel_path.rs` (or equivalent — add
   one if absent) and demonstrate < 5% regression on the kernel-
   fast-path path. If regression > 5%, we DO add `RequestPrototypeSlot`
   in Phase 2 ahead of MAC-02. Same for Response.

9. **Snapshot stability across rustc / quote-crate versions.** `insta`
   snapshots are format-sensitive in two ways:
   (a) rustc's pretty-print of macro-expanded code can shift between
       toolchain versions. The workspace pins rustc via
       `rust-toolchain.toml`; bumping rustc requires `cargo insta accept`
       (with reviewer audit) to refresh snapshots.
   (b) The `quote` crate's whitespace handling has historically had
       benign tweaks across minor versions. Mitigation: pin `quote`
       to a specific minor version in `crates/runtime-macros/Cargo.toml`
       (`quote = "=1.0.x"` rather than `quote = "1"`). Bumping `quote`
       is a dedicated PR with snapshot refresh.
   Both gates are documented in `crates/runtime-macros/README.md`
   (added during Phase 1 PR).

---

## 9. Decision summary

- **Recommended option:** B (state-shaped impl, marker named via
  `#[v8_state_marker(MarkerTy)]`). Rationale: receiver types match
  what's behind the pointer; composable with all existing impl-block
  attributes; backward-compatible (additive attribute); aligns with
  existing codebase convention (CloseEventState / AbortSignal pattern).
- **Codegen change:** ~120 LOC in `crates/runtime-macros/src/v8_class.rs`,
  threading two idents (`state_ty`, `marker_ty`) through the existing
  emit functions per the substitution table in §4.1. 22 emit-change
  sites + 2 invariants under propagation.
- **Risk:** low for the macro change itself (no-attribute path is a
  byte-identical no-op modulo row 16's qualified Private name; locked
  by insta snapshots). Medium for the Request/Response migrations
  (preserve hand-roll's spec deviations; verify against WPT and via
  benchmarks for the kernel fast path).
- **Reward:** unblocks ~-535 LOC net across Request + Response (Request
  -390 per §7.2.5; Response -145 per §7.3.4). Revised down from the
  brief's -1200 LOC target — the floor is set by the spec walks
  (~560 LOC of WebIDL §5.4 / §5.5 logic that migrates verbatim into
  the constructor body and can't be macro-eliminated). Enables MAC-08
  (same-object project = field), MAC-02 (constructor post-init),
  MAC-07 (`#[v8_static_method]`).
- **Bound on follow-up work:** ~7 hours of focused work to land macro
  + both consumers + WPT + benches. Per
  `feedback_estimates_hours_not_weeks`.
- **CI gates** for the landing PR(s):
  1. All 70+ existing `v8_*_smoke.rs` tests pass.
  2. Insta snapshots match (one diff allowed: row 16's Private name).
  3. `wpt_fetch_request.rs`, `wpt_fetch_response.rs`,
     `wpt_fetch_body.rs`, `wpt_fetch_basic.rs`,
     `wpt_fetch_basic_network.rs`, `wpt_fetch_redirect.rs`,
     `wpt_fetch_abort.rs`, and `wpt_headers.rs` pass-rate not
     regressed (full table in §6.3).
  4. Benchmark gate: run `crates/runtime/benches/run_zerobench.sh` (HTTP
     scenarios that exercise build_kernel_request) — regression on the
     `fetch-echo` and `static` scenarios must be < 5% on req/s and
     < 10% on p99 latency, measured against the immediately-preceding
     commit on the PR (i.e., baseline = first commit of the PR
     pre-substitution; gate = final commit post-substitution). The
     standard bench box is whatever `benches/results-2026-04-30-cross-runtime.txt`
     was produced on. If `crates/runtime/benches/` lacks a focused
     kernel-path micro-bench at migration time, add one
     (`fetch_kernel_path.rs` allocating Request and Response wrappers
     in a tight loop, ~50 LOC). Per
     `feedback_estimates_hours_not_weeks`, this is < 1 hour.
