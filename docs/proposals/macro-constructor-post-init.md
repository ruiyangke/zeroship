# MAC-02 — `#[v8_constructor(post_init = "fn")]` with `(scope, this)` signature

**Date:** 2026-05-04
**Status:** **Shipped** — `post_init` codegen lands in `crates/runtime-macros/src/v8_class/parse/marker_attr.rs` and is consumed by `runtime-macros-refactor.md` Wave 1. Document retained as design spec.
**Tracking:** Macro audit MAC-02
**Spec:** WebIDL §3.7.1 (Interface constructors); Streams §3.4 / §3.5 / §4.4 / §5.2 (constructor algorithms); ECMA-262 `[[Construct]]` (§9.4.3 / §10.3.2)
**Affected crate:** `crates/runtime-macros/`
**Consumers blocked:** `crates/runtime/src/web/streams/{readable_default_reader,readable_byob_reader,writable_writer,transform}.rs`; parts of native `Request`/`Response` migration
**V8 crate pin:** workspace `Cargo.toml` declares `v8 = "147"`,
resolving to `v8 v147.1.0` per `Cargo.lock`. The crate is named `v8`
on crates.io (modern naming; the older `rusty_v8` crate name is
deprecated). The runtime imports it as `use v8::...`. The `v8` crate
tracks V8's upstream release train aggressively — major version bumps
frequently. The proposal relies on stable APIs:
`Object::SetInternalField`, `Weak::with_guaranteed_finalizer`,
`FunctionTemplate::new`, the `[[Construct]]` callback contract.
These have been stable across recent v8-crate major versions; the
proposal does NOT depend on any post-147 features.
**Compatibility commitment:** if a future v8-crate upgrade changes
the finalizer or internal-field API, MAC-02 evolves in lockstep with
the existing constructor codegen (which uses the same APIs) — no
additional lock-in introduced by this proposal.
**MSRV:** unchanged (uses only stable `syn` APIs:
`syn::Meta::NameValue`, `syn::Error::to_compile_error()` —
both stable since syn 2.0, which the workspace already requires).

> Round 2 — major restructure. v1's mid-doc reversal between `&Self` and
> `(scope, this)`-only signatures resolved in favour of the latter (§5.5).
> v1's fabricated `OpErrorKind::JsValue` variant in §4.3 corrected against
> the actual `crates/runtime/src/core/state.rs` enum (5 variants).
> v1's LoC estimates re-measured with `wc -l`. Worked AbortSignal example
> added (§5.4). WebSocket-vs-streams comparison table added (§1.5).

---

## Why this exists in two sentences

`#[v8_constructor]` today expects the user body to return a `Self` that the
macro then *boxes and installs* into V8 internal-field-0. Several WHATWG
classes (Streams' Reader, Writer, BYOBReader, TransformStream; Request's
SameObject `headers` cache) need to perform **V8 API calls keyed off the
fully-installed wrapper object** (call helpers that themselves brand-check or
read-back via `with_state`) at construction time, but the macro gives no place
to do that wiring **after** the box install.
This proposal adds an opt-in `post_init = "fn"` attribute on
`#[v8_constructor]` that runs a user-supplied hook **after** the box is
installed in internal-field-0 and **before** the constructor returns to JS,
unblocking those four migrations.

---

## 1. Problem framing

### 1.1 The four blocked stream classes

Each of these has the same shape: a constructor receives a stream, allocates
a paired `(Promise, PromiseResolver)`, stashes the resolver in Rust state, and
writes the Promise into one or more private symbols on the wrapper.

#### Reader — `crates/runtime/src/web/streams/readable_default_reader.rs` (742 LOC)

Lines 271–299 (`set_up_default_reader`): allocate `closed_resolver`, build
`DefaultReaderState::new(resolver_g)`, box-into-raw, set External into field
0, register weak finalizer, then call `readable_stream_reader_generic_initialize`
which writes priv-sym slots (`STREAM`, `READER`, `CLOSED_PROMISE`) on the
reader and pre-resolves/rejects the closedPromise based on stream state.

The constructor body is ~70 lines of receiver checks (lines 196–240) plus the
hand-rolled installation. The **`#[v8_class]` macro currently can't express
this** because the priv-sym writes don't strictly need post-install ordering
(priv-syms don't brand-check), but
`readable_stream_reader_generic_initialize` is shared between this constructor
and `acquireReader` machinery; pulling it apart on the in-body-only path
would fork that helper. Post_init keeps the helper intact.

#### BYOBReader — `crates/runtime/src/web/streams/readable_byob_reader.rs` (894 LOC)

Lines 296–360 / 407–454. Same pattern. Plus an extra brand-tag write
(`BYOB_READER_TAG_SLOT`) that distinguishes the BYOB box from the
default-reader box (since both have an External in field 0 — the macro's
brand check would handle this for us once migrated, but the hand-roll
explicitly stamps the tag).

#### Writer — `crates/runtime/src/web/streams/writable_writer.rs` (795 LOC)

Lines 175–223 / 250–339. **Two paired Promises** — `[[closedPromise]]` AND
`[[readyPromise]]` — and the initialization branches by `WSState`
(`Writable` / `Erroring` / `Closed` / `Errored`). Several branches need
`make_pending_promise` / `make_resolved_promise` / `make_rejected_promise`
(lines 345–363) and to write each into a priv-sym slot. The resolvers go
into `WriterState`. Like Reader, the SetUp helper is shared with
`acquireWriter`; post_init keeps the helper intact across the two callers.

#### TransformStream — `crates/runtime/src/web/streams/transform.rs` (905 LOC)

Lines 293–391. The `TransformStream` constructor:
1. Validates `transformer.readableType` / `writableType` are not set
2. Allocates a `start_promise` resolver pair
3. Builds a `Box<TSStreamState>` with `bp_change_promise` / `bp_change_resolver`
4. Writes the brand priv-sym `[[ts.brand]]`
5. Calls `set_up_transform_stream_default_controller_from_transformer` which
   internally calls `build_readable_for_ts` and `build_writable_for_ts` —
   both of which **construct OTHER classes** (a `ReadableStream` and a
   `WritableStream`) keyed on `ts: v8::Local<v8::Object>` (the TS wrapper).

This last step is where post-init *truly* helps. The
`set_up_transform_stream_default_controller_from_transformer` call needs the
TS wrapper to **already exist** with its internal field installed and
brand priv-sym written, because the readable/writable halves capture
`v8::Global::new(scope, ts)` and the controller's start algorithm reads
back from the TS state via `with_ts_state` (which itself runs an
`is_transform_stream` check that consults the brand priv-sym).

### 1.2 Why a `wrapper: Local<Object>` synthetic in the user constructor isn't enough

The macro already supports a synthetic `wrapper: v8::Local<v8::Object>`
parameter that gets bound to `args.this()` (see `is_wrapper_local` in
`crates/runtime-macros/src/v8_class.rs:1882`; consumed today by
`crates/runtime/src/web/websocket/mod.rs:301`). One could naively claim:
"add the synthetic, run all the resolver/priv-sym writes inside the user
constructor body, return `Self` — done." That works for Reader / BYOBReader /
Writer, where the user wiring only needs `&mut PinScope` and `args.this()`
without ever touching the box.

It **does not** work for TransformStream, because:

- `set_up_transform_stream_default_controller_from_transformer` reads the
  TS state via `with_ts_state(scope, ts, |s| ...)`. That helper does
  `is_transform_stream(scope, ts)` first, which checks the `[[ts.brand]]`
  priv-sym. The priv-sym write is sequenceable before the box install
  (so brand check itself works), BUT
- The same call's downstream code path (`build_readable_for_ts`,
  `build_writable_for_ts`) constructs *other* JS objects whose start
  algorithms can run synchronously and **call back into the TS** via
  `ts_controller_slot(scope, ts)`. Those reads consult priv-syms — which
  haven't been written yet at the point where `set_up_…_from_transformer`
  runs **inside** the user constructor body, because the constructor body
  hasn't yet returned the `Self` to be boxed.
- More fundamentally: any chain of construction calls that ends up calling
  *prototype methods* on the TS wrapper would brand-check (per WebIDL §3.7,
  enforced by the macro's `__brand_check_TransformStream`), and the brand
  check looks for a non-null External in field 0 + a prototype-chain
  match on `TransformStream.prototype`. Field 0 isn't installed yet.

So the **correct factoring** is: the user constructor returns the
data-only `Self` (sliced bytes, optional resolvers, the budget guard); the
macro installs the box; **then** the macro calls a post-init hook that
runs `set_up_transform_stream_default_controller_from_transformer(scope,
this, transformer, …)`. By that point the TS wrapper is fully live: brand
priv-sym written (within post_init), internal field 0 set, prototype
already chained via the FunctionTemplate.

### 1.3 Why this is also Request migration's blocker

`Request` and `Response` migrations have a similar issue: `Request.headers`
needs to mint a `Headers` JS object (a separate `#[v8_class]` instance)
keyed off the request wrapper, and the hand-rolled construction today
calls `Headers::new` *after* the request is fully wrapped. Migrating
`Request` to `#[v8_class]` means giving the request constructor a place
to call `Headers::new` and stash the result on a priv-sym (`[SameObject]`
caching, see `gen_same_object_getter_callback` in
`crates/runtime-macros/src/v8_class.rs:1219`). That is *exactly* what
post-init enables.

### 1.4 What the macro can do today

`crates/runtime-macros/src/v8_class.rs:1623` (`gen_constructor_callback`)
emits:

```text
fn __Foo_constructor_callback(scope, args, _rv) {
  must-new check                  // 1
  let __this = args.this();
  <JS arg extractions>            // 2
  <synthetic scope/wrapper bind>  // 2b
  let __instance = Self::ctor(extracted args)  // 3 — user body
  // gen_box_and_install_finalizer (line 1722):
  let __boxed = Box::new(__instance);
  let __raw = Box::into_raw(__boxed);                 // 4a
  let __ext = v8::External::new(scope, __raw);
  __this.set_internal_field(0, __ext.into());         // 4b — box live in V8
  v8::Weak::with_guaranteed_finalizer(scope, __this, |__raw| drop)  // 5
  std::mem::forget(__weak);                           // 5b
  // (no return — V8 implicitly returns args.this() per ECMA-262 §10.3.2:
  //  if the FunctionCallback writes nothing to ReturnValue, the construct
  //  result is args.this(); writing a non-Object is ignored; writing an
  //  Object would override args.this(). The macro never writes _rv.)
}
```

There is no slot for "run user code AFTER step 5b". This proposal adds one.

### 1.5 WebSocket vs streams: a side-by-side

The macro already has the `wrapper: Local<Object>` synthetic param (used by
WebSocket). Why not also use it for streams? The difference is **what the
constructor body needs to do with `this`**:

| Aspect | WebSocket | Streams (Reader/Writer/BYOB/TS) |
|---|---|---|
| When does the ctor body need `this`? | Pre-box-install (in user ctor body) | Post-box-install (after field-0 set) |
| What does the ctor body do with `this`? | Stash `v8::Global::new(scope, this)` so dispatch can wake the wrapper later | Pass to helper that brand-checks + reads via `with_state` |
| Does any helper call back into `this`? | **No** — only Global capture | **Yes** — `with_ts_state`, `with_state(this, ...)` |
| Does any helper brand-check `this`? | **No** | **Yes** (TransformStream's `is_transform_stream` priv-sym + downstream method calls) |
| Does any helper read field 0? | **No** | **Yes** (`with_state` recovers the box) |
| Workable with `wrapper:` synthetic alone? | YES | NO (TransformStream needs box live) |
| Workable with `post_init` hook alone? | YES | YES |

WebSocket's pattern (capture-Global before box install) is **safe for that
class** because the dispatch arm only needs the wrapper identity for later
event delivery — it never reads field 0 during construction. Streams
helpers DO read field 0 (or brand-check via the prototype chain pinned at
install time + private symbols). Hence post_init.

**Decision: keep `wrapper:` synthetic for WebSocket back-compat. Streams
take the post_init route. A class can use BOTH** (capture wrapper Global
in the user body, do post-install wiring in the hook) if it wants —
ordering is "wrapper synthetic bind → user body → box install → post_init",
unambiguous from §1.4.

### 1.6 Out of scope

- Pre-init hooks (would run BEFORE the user `ctor()`). Not motivated by
  any consumer; the must-new check + arg extraction is the existing
  pre-body sequence.
- Async constructors. Spec-impossible: WebIDL §3.7.1 is sync. Async setup
  belongs in a follow-up `start()`-style method on the instance.
- Constructor return-`Promise`. Same reason.
- Multi-hook ordering (e.g. `pre_init` + `post_init`). If MAC-NN later
  introduces `pre_init`, ordering is fixed: `pre_init` runs after `must_new`
  and before extractions; `post_init` runs after box install. No reorder
  possible — both points are cited from spec sections (§3.7.1 step 3 and
  step 7 respectively).

---

## 2. Design adequacy — three options + recommendation

### Option A — `this: v8::Local<v8::Object>` AS a magic param

Extend `is_wrapper_local` (already present at line 1882) to apply to
`#[v8_constructor]` bodies as well. The user body becomes:

```rust
#[v8_constructor]
fn new(
    scope: &mut v8::PinScope,
    this: v8::Local<v8::Object>,
    stream: v8::Local<v8::Value>,
) -> Result<DefaultReader, OpError> {
    // ... validate stream ...
    let resolver = v8::PromiseResolver::new(scope).unwrap();
    let promise = resolver.get_promise(scope);
    slots::write_slot(scope, this, CLOSED_PROMISE, promise.into());
    let resolver_g = v8::Global::new(scope, resolver);
    Ok(DefaultReader { closed_resolver: RefCell::new(Some(resolver_g)) })
}
```

**Cost:** The user body runs **before** the box install (step 4 in §1.4).
So:
- Priv-sym writes on `this` work (priv-syms don't brand-check).
- `set_internal_field` on `this` would *succeed silently* but be **clobbered**
  by the macro's subsequent `__this.set_internal_field(0, __ext.into())`
  (step 4b). Not a problem — no consumer wants to set their own External.
- Method calls on `this` would brand-check fine (the brand check walks the
  prototype chain, not field 0; the prototype was set when V8 instantiated
  the wrapper from the ObjectTemplate before the callback fired). **But**
  any method body would then try to recover the box from field 0 and find
  an empty External → "Illegal invocation" by the existing prologue.
- Calls into helpers like `set_up_transform_stream_default_controller_…`
  that themselves call `with_ts_state(scope, this, ...)` would fail —
  the box isn't installed yet.

So Option A unblocks Reader / BYOBReader / Writer (which only need
`PromiseResolver::new` + priv-sym slots), but **NOT TransformStream**
(needs the box live to call the controller-setup helper).

### Option B — `#[v8_constructor(post_init = "fn_name")]` with `(scope, this)`

The macro recognises `post_init = "..."` on `#[v8_constructor]`. The named
function lives on the same impl block, takes `(scope, this)` — **no `&Self`
parameter** — and runs after the box install:

```text
fn __Foo_constructor_callback(scope, args, _rv) {
  must-new check
  let __this = args.this();
  <JS arg extractions>
  <synthetic scope/wrapper bind>            // includes `this:Local<Object>` if user opted in
  let __instance = Self::ctor(extracted args)
  // gen_box_and_install_finalizer:
  let __boxed = Box::new(__instance);
  let __raw = Box::into_raw(__boxed);
  let __ext = v8::External::new(scope, __raw);
  __this.set_internal_field(0, __ext.into());
  v8::Weak::with_guaranteed_finalizer(...);
  std::mem::forget(__weak);

  // POST_INIT (NEW):
  match Self::after_install(scope, __this) {
    Ok(()) => {},
    Err(__err) => {
      <error throw, mirroring the user-ctor Result<Err> handling exactly>
      return;
    }
  }
}
```

The hook signature is:
```rust
fn after_install(
    scope: &mut v8::PinScope,
    this: v8::Local<v8::Object>,
) -> Result<(), OpError>;
```

**Critical: the hook does NOT receive `&Self`.** To read box state, the
hook calls the class's existing `with_state(scope, this, |s| ...)` helper —
which goes through field-0 External, with the same null/missing handling
already used by every method on the class. This idiom is the standard
pattern across all four streams classes today.

**Why no `&Self`?** A `&Self` materialised by the macro at post_init time
would alias any `&mut Self` recovered by a reentrant `&mut self` method
call (post_init can call into JS via PromiseResolver allocation; a
side-channel — a saved reference to `Reflect.apply` capturing
`args.this()` via a getter trap on the prototype chain — could reach a
method on this same instance). The existing per-method re-entry guard
(`gen_reentry_guard`, line 1024) catches **method-vs-method** aliasing
but doesn't see the macro-emitted `&Self` borrow at post_init level. The
safe path is: don't materialise a borrow at all; let the user reach the
state through `with_state`, which goes through the External recovery
path that the re-entry guard understands.

**Cost:** A separate fn (not a closure / not inline). The state is reached
via `with_state` — same constraint as `&self` methods. The hook can call
into helpers that read the box back via `with_state(scope, this, …)`
(those go through the External in field 0; the box is live).

### Option C — Both A and B available simultaneously

Available simultaneously: in-body wiring (priv-syms, brand tags) via A;
post-box-install wiring (calls that need `with_state` / brand-checked
methods) via B.

### Recommendation: **Option B alone**

Rationale:

1. **Post-init subsumes the use cases of pre-init.** Anything Option A can
   do (write a priv-sym, allocate a Promise, call a no-this helper) can
   ALSO be done from post_init. The reverse is not true (TransformStream).
2. **Single mental model.** Two hooks doubles the documentation surface;
   most consumers only need one. Reader / BYOBReader / Writer migrate
   cleanly with post_init alone.
3. **Forces the right factoring.** "User code that needs the wrapper
   identity goes in a separate fn" matches WebIDL spec algorithms (the
   spec splits constructors into "step 1: convert IDL types", "step 2:
   set up the object" — `after_install` IS step 2).
4. **Lowest macro risk.** Adds one attribute, one codegen block at a
   single insertion point (immediately after `#store` in
   `gen_constructor_callback`). Doesn't touch the existing
   `is_wrapper_local` logic for methods, doesn't change the user-body
   extraction order, doesn't interact with `gen_async_method_callback`
   (constructors are sync — there are no async constructor codepaths to
   collide with; see §1.6).
5. **Migration path is a strict refactor.** Streams migrations move the
   "everything after `Box::into_raw` + `set_internal_field`" portion of
   the hand-roll into the post_init fn unchanged. No spec-deviation risk.
6. **Re-entrancy safety by construction.** No `&Self` from the macro
   means no aliasing surface at the macro layer (see §5.5 deep-dive).

Option C is rejected because the only argument *for* it ("convenience")
fails: the streams cases all fit into post_init alone. WebSocket's
existing `wrapper: Local<Object>` synthetic for the user constructor stays
supported (back-compat) but isn't extended to apply to streams; streams
take the cleaner post_init route. A class that wants both A and B can
combine them today (§1.5) — the macro doesn't need to grow a new feature
to permit it.

---

## 3. Spec correctness

### 3.1 WebIDL §3.7.1

The spec for "interface objects ... [[Construct]]" leaves the
"create-and-initialize" sequence opaque — implementations are required to
"set up the new object" before returning to JS. The split between
`Self::ctor` (returns data) and `Self::after_install` (wires V8-side
storage) is a faithful realisation of *one* allowed sequencing. The spec
mandates only that:

- The constructor's `[[Realm]]` is consulted (V8 handles).
- The newly-created object's `[[Prototype]]` is the constructor's
  `prototype` (V8 + FunctionTemplate handles).
- The constructor returns the new object — per ECMA-262 §10.3.2 step 12,
  if `kind` is `"derived"` and the result of executing the function body is
  an Object, that Object is returned; otherwise `args.this()` is returned.
  The macro never writes to `_rv`, so V8 always returns `args.this()`.

None of these are affected by post_init. Post_init runs **inside** the
"set up the new object" slice, between V8's "allocate object via
ObjectTemplate" and V8's "return args.this() to the caller". This is the
same window the hand-rolled stream constructors already use today.

### 3.2 Reentrancy

Post_init runs while V8's "construct" frame is still open. Single-threaded
single-isolate (V8's invariant — the runtime never shares an isolate
across compio threads, see `crates/worker/src/cache.rs`). All ordering
arguments below are within a single thread, single isolate.

JS hasn't received the wrapper yet — but **a JS callback could fire
from inside post_init** if the hook uses any V8 API that runs JS:

- `PromiseResolver::resolve(scope, value)` queues a microtask but does
  NOT synchronously re-enter user JS; microtasks drain at the next
  microtask checkpoint (after the constructor callback returns to V8).
  **Safe.**
- `Local<Function>::call(scope, ...)` invokes JS synchronously. Can run
  user code that may rediscover `args.this()` via a side channel (e.g.
  the constructor leaked it to a closure during arg extraction via a
  proxy trap on `[[Get]]`). **Hazardous, but no streams hook calls user
  JS synchronously.**
- `Object::get` / `set` on `this` itself — could trigger user-defined
  accessors on the prototype (none exist for streams classes; safe for
  this proposal).

The wrapper's state at post_init entry:

- Internal field 0 is set (step 4b from §1.4).
- Brand priv-syms written by the user's `Self::ctor` body (none in the
  streams cases) are present; brand priv-syms the hook plans to write
  are not yet present.
- The prototype chain matches `Foo.prototype` (V8 set this when allocating
  via ObjectTemplate before the callback fired).

So if a side-channel reaches a `Foo` method on this same wrapper from
inside post_init:

1. The brand check passes (prototype chain match).
2. The External recovery succeeds (field 0 set).
3. The method materialises `&mut Self` (or `&Self`) from the External.
4. **No conflicting borrow at the macro layer**, because the macro emits
   no borrow in post_init's prologue (per §2.B; this is the
   re-entrancy-safety argument for Option B's signature).
5. If the method itself is `&mut self`, the per-method re-entry guard
   (line 1024) is the only protection; post_init does NOT count as a
   prior `&mut self` materialisation, so the guard sees zero prior
   entries and admits the call. **This is correct** as long as post_init
   itself doesn't separately materialise `&mut Self` (it doesn't —
   `with_state` returns `Option<R>` from a closure that takes `&Self`,
   not `&mut Self`).

Conclusion: post_init is reentrancy-safe **provided** the hook accesses
state only through `with_state` (or equivalent `&Self`-only borrows
through the External). Hooks that need mutation MUST go through interior
mutability (`Cell` / `RefCell`) inside the box, the same as `&self`
methods today.

### 3.3 Streams §3.4 / §3.5 / §4.4 / §5.2

Each Streams class spec mandates a SetUp algorithm that runs at
construction time:

- §3.4.5 `SetUpReadableStreamDefaultReader` — runs `ReaderGenericInitialize`
  which sets `[[stream]]`, `[[reader]]`, `[[closedPromise]]`. The
  closedPromise is pre-resolved or pre-rejected based on the stream's
  state. ALL of this can run in post_init.
- §3.5 `SetUpReadableStreamBYOBReader` — same shape; plus the BYOB-only
  brand tag.
- §4.5.4 `SetUpWritableStreamDefaultWriter` — branches on
  `WritableStream.[[state]]` and writes either pending or pre-settled
  Promises into priv-sym slots; resolvers go into `WriterState`. Fits
  post_init.
- §5.2.3 `InitializeTransformStream` + §5.4.2 `SetUpTransformStreamDefault
  ControllerFromTransformer` — the longest sequence; calls into
  `build_readable_for_ts` / `build_writable_for_ts` which themselves
  construct fresh `ReadableStream` / `WritableStream` instances. Fits
  post_init (the box is live, with_state resolves cleanly).

No spec algorithm REQUIRES "wrapper-identity-dependent state setup happen
BEFORE the box is installed". The single-call hand-rolled constructors
today are an artifact of the macro's lack of post_init, not a spec
mandate.

### 3.4 Brand check in post_init

Step ordering in the post_init regime:

```text
1. V8 calls ctor callback                                     [JS frame open]
2. macro: must-new check
3. macro: extractions
4. macro: synthetic bind (scope, optional `wrapper:`)
5. macro: user body (returns Self or Err)                     [Self exists in Rust stack]
6. macro: Box::into_raw                                       [Self moved into heap Box]
7. macro: ext = External::new + set_internal_field(0, ext)    [box live in V8 field 0]
8. macro: with_guaranteed_finalizer + mem::forget             [finalizer registered]
9. macro: POST_INIT — Self::after_install(scope, this)        [first chance for brand-checked method calls]
10. macro: callback returns                                   [V8 returns args.this() to JS caller]
```

At step 9, both:

- The prototype chain matches `Foo.prototype` (V8 set this when allocating
  via ObjectTemplate in step 1).
- The brand-check helper's lazy initialization (line 545–589 of
  v8_class.rs) reads `__InstallSlot_Foo` to derive the cached prototype.
  **Precondition:** `Foo::install(scope)` MUST have run on this isolate
  before any `new Foo()`. This is guaranteed by the macro: the
  FunctionTemplate that V8 dispatches to in step 1 is created inside
  `Foo::install` and registered on the isolate's global; if `install`
  hadn't run, V8 would have no FunctionTemplate to call, so the callback
  fires only after install. Stated explicitly so future contributors don't
  accidentally break the invariant.
- The internal field is set (step 7), so any method call's
  `__ext = __this.get_internal_field(scope, 0)` returns Some(External).

So the brand check passes from inside post_init. The `unsafe { &mut *
(__ext.value() as *mut Self) }` materialisation in a reentrant method
call **does NOT alias** any macro-emitted borrow because (per §2.B) the
macro emits no borrow in post_init. Reentry is sound.

### 3.5 V8 docs on `Object::SetInternalField` ordering

`v8.h` `Object::SetInternalField` writes directly to the inline embedder-data
slot on the JSObject (no caching, no lazy publish). In V8 source: the call
chain is `v8::Object::SetInternalField` →
`v8::internal::JSObject::SetEmbedderField` (in `src/objects/js-objects.h`)
→ direct `WRITE_FIELD` macro to a fixed offset. There is no "publish fence"
because there's no separate visibility stage; the write is a plain store
that the same isolate's JIT will read on the next access.

rusty_v8's `Object::set_internal_field` is a thin shim
(`crates/runtime/v8.rs` mirrors the V8 ABI). We're single-threaded,
single-isolate. By the time the JIT reaches step 9, step 7's write is
observable to any subsequent `get_internal_field`. Confirmed empirically
by the existing `set_up_default_reader` hand-roll: it relies on the same
ordering today (set the field, then call
`readable_stream_reader_generic_initialize` which `with_state`s back).

### 3.6 ECMA-262 [[Construct]] semantics

Per ECMA-262 §10.3.2 (`OrdinaryConstruct`) step 12: if the function's
body returns an Object, that Object is returned to the caller. Otherwise,
the bound `this` is returned. V8's `FunctionCallback` mirrors this: if
`ReturnValue::Set` is called with an Object, V8 uses that as the
construct result; otherwise `args.this()` is the result.

The macro never writes to `_rv` (verified: `gen_constructor_callback`
emits no `rv.set(...)` call). Therefore every macro-emitted constructor
returns `args.this()`. **This is intentional and load-bearing**: post_init
must not write to `_rv` either. Document this in the proc-macro doc-string.

---

## 4. Codegen feasibility

### 4.1 Current `gen_constructor_callback`

Located at `crates/runtime-macros/src/v8_class.rs:1623`. The emitted
function (with line refs from the macro source):

```text
1623 fn gen_constructor_callback(class_ty, c) -> TokenStream2 {
1624-1632   parse user signature, get extractions, call_args
1634-1637   detect Result return (is_result)
1639-1665   build `make_instance` block — Result-handling (lines 1640-1660) or plain (1662-1664)
1667        gen_box_and_install_finalizer(class_ty)        // step 7+8 above
1668        gen_must_new_prologue                           // step 2
1670-1685   final quote! { … }
              must_new
              let __this = args.this();
              extractions
              make_instance
              store    ← step 7 + 8
            }
```

The proposed insertion point is **immediately after `#store`** in the
final `quote!`. The post_init call gets emitted as a separate fragment
that's empty when the user didn't opt in.

### 4.2 Attribute parsing

Add a helper next to `extract_callable_no_new` (line 1569):

```rust
/// Read `#[v8_constructor(post_init = "fn_name")]`. Returns the named
/// fn (as `syn::Ident`) or None if absent. Emits `compile_error!` on
/// malformed shapes (non-string literal, non-identifier value, etc.) —
/// see §5.7 for the rationale (post_init is semantically load-bearing;
/// silent no-op would be a debugging nightmare).
fn extract_post_init(attrs: &[Attribute])
    -> Result<Option<syn::Ident>, syn::Error>
{
    for attr in attrs {
        if !attr.path().is_ident("v8_constructor") {
            continue;
        }
        // Parse the argument list as a Punctuated<Meta, Comma>.
        // Two shapes coexist: bare ident (`callable_no_new`) and
        // name=value (`post_init = "fn"`). We filter to the
        // NameValue { path == "post_init" } variant.
        let parsed = attr.parse_args_with(
            syn::punctuated::Punctuated::<syn::Meta, syn::Token![,]>::parse_terminated
        )?;
        for meta in parsed {
            if let syn::Meta::NameValue(nv) = meta {
                if !nv.path.is_ident("post_init") { continue; }
                let lit = match &nv.value {
                    syn::Expr::Lit(syn::ExprLit { lit: syn::Lit::Str(s), .. }) => s,
                    _ => return Err(syn::Error::new_spanned(
                        &nv.value,
                        "post_init must be a string literal naming a function on this impl, e.g. post_init = \"after_install\"",
                    )),
                };
                let raw = lit.value();
                let ident = syn::parse_str::<syn::Ident>(&raw).map_err(|_| {
                    syn::Error::new_spanned(
                        lit,
                        format!("post_init = {raw:?} is not a valid Rust identifier"),
                    )
                })?;
                return Ok(Some(ident));
            }
        }
    }
    Ok(None)
}
```

The attribute syntax is **list form** with mixed shapes:
`#[v8_constructor(callable_no_new)]` already exists; we add
`#[v8_constructor(post_init = "after_install")]`. They can be combined:
`#[v8_constructor(callable_no_new, post_init = "after_install")]`.

If parsing fails, the macro emits the `compile_error!` at the precise
span of the offending value (via `syn::Error::to_compile_error()`).

### 4.3 Emitted post_init block

```rust
// Inside gen_constructor_callback, after `let store = …;`:
let post_init = match extract_post_init(&c.func.attrs) {
    Ok(None) => quote! {},
    Err(e) => return e.to_compile_error(),
    Ok(Some(hook_ident)) => quote! {
        // Mirrors the make_instance Result arm verbatim — same five
        // OpErrorKind variants from crates/runtime/src/core/state.rs:32
        // (TypeError, RangeError, Error, DomException(name),
        // NodeError(code)). Any addition there must be mirrored here.
        match <#class_ty>::#hook_ident(scope, __this) {
            Ok(()) => {},
            Err(__err) => {
                let __msg = v8::String::new(scope, &__err.message).unwrap();
                let __exc: v8::Local<v8::Value> = match __err.kind {
                    ::zeroship_runtime::state::OpErrorKind::TypeError =>
                        v8::Exception::type_error(scope, __msg),
                    ::zeroship_runtime::state::OpErrorKind::RangeError =>
                        v8::Exception::range_error(scope, __msg),
                    ::zeroship_runtime::state::OpErrorKind::DomException(__name) => {
                        ::zeroship_runtime::dom::exception::build(
                            scope, &__err.message, __name).into()
                    }
                    ::zeroship_runtime::state::OpErrorKind::NodeError(__code) => {
                        ::zeroship_runtime::node_error::build_node_exception(
                            scope, __code, &__err.message)
                    }
                    ::zeroship_runtime::state::OpErrorKind::Error =>
                        v8::Exception::error(scope, __msg),
                };
                scope.throw_exception(__exc);
                return;
            }
        }
    },
};
```

…and in the final `quote!` at line 1670:

```rust
quote! {
    pub(crate) fn #callback_ident(...) {
        #must_new
        let __this = args.this();
        #(#extractions)*
        #make_instance
        #store
        #post_init       // <-- NEW; empty TokenStream2 when not opted in
    }
}
```

### 4.4 Box reclamation when post_init throws

When post_init returns `Err`, the macro throws a JS exception and
returns from the callback. The wrapper is now half-constructed:

- Internal field 0 is set (step 7 ran), pointing at a live Box.
- A weak finalizer is registered (step 8 ran) and `mem::forget`'d so
  it persists.
- JS never receives the wrapper (the callback threw; V8 propagates the
  exception to the `[[Construct]]` caller; the construct expression
  evaluates to abrupt completion).

The wrapper has no JS roots after the throw and no internal C++ root
beyond the WeakHandle (which is, by definition, weak). On the next major
or minor GC, V8 reclaims the wrapper and fires the finalizer, which
drops the Box. **The wrapper does NOT necessarily get reclaimed
synchronously** — V8's garbage collector is incremental and lazy. On a
quiet isolate the next minor GC may be milliseconds away; on a busy
isolate it could be seconds.

**Side-effect timing.** Critically: any `Drop` impl on `Self`
(e.g. the streams budget guard's `Drop` decrements an
`AtomicUsize` counter) does NOT fire until GC. This is a known
V8-embedder footgun: `Drop` semantics in Rust are eager, but V8
finalizers are lazy. **For v1, this is documented as an explicit
non-feature: `Self::Drop` side effects from a failed post_init may
be delayed by up to one GC cycle (typically <1s, worst case bounded
by isolate lifetime). Hooks that allocate "important" resources
(file descriptors, network handles, etc.) MUST clean up explicitly
in the Err path before returning, OR keep those resources outside
`Self`.**

**Memory pressure surface:** if a class' post_init has a non-trivial
failure rate (e.g. validation in `set_up_transform_stream_…`), the
half-constructed wrappers + boxes accumulate between GCs.

- `DefaultReaderState` is ~64 bytes → 16 bytes of wrapper + ~64 of box ≈
  80 bytes per failure.
- `TSStreamState` carries a budget guard + several Globals ≈ 256 bytes
  per failure.

For a worst-case 1k failures/sec on TransformStream, 256 KB pile up
between GCs. Acceptable for the current platform budget (workers are
1 vCPU, ~256 MB heap). **No fix needed in v1** for memory pressure.

**Eager-drop alternative considered.** The macro could emit, in the Err
arm:

```rust
// Hypothetical eager-drop path:
let __ext_undef = v8::undefined(scope).into();
__this.set_internal_field(0, __ext_undef);
unsafe { drop(Box::from_raw(__raw_ptr)); }
```

This would fire `Self::Drop` immediately. But:

1. The weak finalizer (registered at step 8) is keyed on the same
   `__raw_addr` and would `Box::from_raw` again at GC — **double-free**.
   Avoiding this requires either (a) skipping finalizer registration
   when `post_init` is opted-in (changes the success path's lifetime
   model), or (b) tracking a "did-we-drop-eagerly" flag through the
   weak callback (adds 1 word per instance).
2. After the field-0 clear, any reentrant method call from a microtask
   queued before the throw would brand-check OK (prototype matches)
   but get `Some(v8::Undefined)` from `get_internal_field(0)`, then
   the existing prologue's "External recovery" path returns "Illegal
   invocation". Hostile path but well-defined.

**Decision:** v1 accepts lazy drop. If post_init failure rates prove
high in production (monitored via the `runtime.v8_class.post_init_errors`
counter from §5.11), MAC-NN can ship the eager-drop path with the
finalizer-skip opt-in. The cost-benefit doesn't justify shipping the
complexity now.

### 4.5 Worked example: Reader migration

**Before** (`crates/runtime/src/web/streams/readable_default_reader.rs`,
lines 196–299, hand-rolled — paraphrased):

```rust
fn constructor_callback(scope, args, _rv) {
    if !args.is_construct_call() { /* throw */ return; }
    let reader_obj = args.this();
    let stream_arg = args.get(0);
    let Ok(stream) = v8::Local::<v8::Object>::try_from(stream_arg) else { /* throw */ return; };
    if !is_readable_stream(scope, stream) { /* throw */ return; }
    if algorithms::is_readable_stream_locked(scope, stream) { /* throw */ return; }
    set_up_default_reader(scope, reader_obj, stream);
}

fn set_up_default_reader(scope, reader, stream) {
    let resolver = v8::PromiseResolver::new(scope).unwrap();
    let closed_promise = resolver.get_promise(scope);
    let resolver_g = v8::Global::new(scope, resolver);
    let state = DefaultReaderState::new(resolver_g);
    let boxed = Box::new(state);
    let raw_ptr = Box::into_raw(boxed);
    // ... ext + set_internal_field + weak finalizer (lines 286–295) ...
    readable_stream_reader_generic_initialize(scope, reader, stream, closed_promise);
}
```

**After** (with `post_init`):

```rust
pub struct DefaultReaderState {
    pub read_requests: RefCell<VecDeque<ReadRequest>>,
    pub closed_resolver: RefCell<Option<v8::Global<v8::PromiseResolver>>>,
    /// Stashed by `new` so `after_install` can wire stream.[[reader]] etc.
    /// Cleared (`take()`) at end of `after_install`. Carrying it in state
    /// (rather than re-fetching from the JS arg list, which `after_install`
    /// can't see) is the §1.6 "args plumbing" pattern — see Open Q #5.
    pub pending_stream: RefCell<Option<v8::Global<v8::Object>>>,
}

#[v8_class]
#[v8_to_string_tag = "ReadableStreamDefaultReader"]
impl DefaultReaderState {
    /// Receiver checks per spec §3.4.4 step 1–3.
    #[v8_constructor(post_init = "after_install")]
    fn new(
        scope: &mut v8::PinScope,
        stream: v8::Local<v8::Value>,
    ) -> Result<DefaultReaderState, OpError> {
        let stream = v8::Local::<v8::Object>::try_from(stream).map_err(|_| {
            OpError::type_error("ReadableStreamDefaultReader: argument must be a ReadableStream")
        })?;
        if !is_readable_stream(scope, stream) {
            return Err(OpError::type_error(
                "ReadableStreamDefaultReader: argument must be a ReadableStream",
            ));
        }
        if algorithms::is_readable_stream_locked(scope, stream) {
            return Err(OpError::type_error(
                "ReadableStreamDefaultReader: stream is already locked",
            ));
        }
        // V8 returns None from PromiseResolver::new only on isolate
        // termination or out-of-memory; map to OpError so the macro's
        // Result arm throws an Error rather than panicking.
        let resolver = v8::PromiseResolver::new(scope)
            .ok_or_else(|| OpError::error("PromiseResolver::new failed (OOM?)"))?;
        let resolver_g = v8::Global::new(scope, resolver);
        Ok(DefaultReaderState {
            read_requests: RefCell::new(VecDeque::new()),
            closed_resolver: RefCell::new(Some(resolver_g)),
            pending_stream: RefCell::new(Some(v8::Global::new(scope, stream))),
        })
    }

    /// Spec §3.9.2 ReaderGenericInitialize. Runs after box install
    /// (see §5.4 for visibility rule; `pub(crate)` lets derived
    /// classes chain explicitly).
    pub(crate) fn after_install(
        scope: &mut v8::PinScope,
        this: v8::Local<v8::Object>,
    ) -> Result<(), OpError> {
        // Recover ctor args + resolver via `with_state` (which goes
        // through field-0 External — the box is now live). `with_state`
        // returns `Option<R>` where R is the closure's return; here R
        // is a tuple `(Option<Global<Object>>, Option<Global<PromiseResolver>>)`
        // because each field is independently taken/cloned. So the
        // overall return is `Option<(Option, Option)>` — NOT
        // `Option<Option<T>>`, hence we can't use `Option::flatten`.
        // Unpack explicitly via two ok_or_else? layers.
        //
        // CAUTION: do NOT call into JS (Local::call, Promise::resolve,
        // Object::get with a getter trap, etc.) inside this closure —
        // the closure holds a `&Self` borrow into the box for its full
        // duration. Reentrant JS could land a `&mut self` method that
        // would alias the borrow. The closure body below is pure Rust:
        // RefCell take/clone, no scope access. (`scope` is captured
        // from the enclosing function, but never used inside the
        // closure.) This convention is enforced by review, not by the
        // type system; see §3.2 / §5.5.
        let (stream_g_opt, resolver_g_opt) = with_state(scope, this, |s| (
            s.pending_stream.borrow_mut().take(),
            s.closed_resolver.borrow().clone(),
        )).ok_or_else(||
            OpError::error("after_install: with_state returned None"))?;
        let stream_g = stream_g_opt.ok_or_else(||
            OpError::error("after_install: missing pending_stream"))?;
        let resolver_g = resolver_g_opt.ok_or_else(||
            OpError::error("after_install: missing closed_resolver"))?;
        let stream = v8::Local::new(scope, &stream_g);
        let resolver = v8::Local::new(scope, &resolver_g);
        let closed_promise = resolver.get_promise(scope);

        // Spec §3.9.2 ReaderGenericInitialize:
        //   reader.[[stream]] = stream
        //   stream.[[reader]] = reader
        //   reader.[[closedPromise]] = closed_promise
        //   pre-resolve / pre-reject closed_promise per stream.[[state]]
        slots::write_slot(scope, this, STREAM, stream.into());
        slots::write_slot(scope, stream, READER, this.into());
        slots::write_slot(scope, this, CLOSED_PROMISE, closed_promise.into());

        // Pre-resolve / pre-reject based on stream state per §3.9.2 step 3-5.
        let st = match crate::streams::readable::with_rs_state(
            scope, stream, |s| s.state.get(),
        ) {
            Some(s) => s,
            None => return Ok(()), // stream is not a ReadableStream — caught by §3.4.4 step 2 already; defensive
        };
        match st {
            StreamState::Readable => {} // pending — nothing to do
            StreamState::Closed => {
                let undef: v8::Local<v8::Value> = v8::undefined(scope).into();
                resolver.resolve(scope, undef);
            }
            StreamState::Errored => {
                let stored = slots::read_slot(scope, stream, STORED_ERROR);
                resolver.reject(scope, stored);
                // Match spec [[PromiseIsHandled]] = true behaviour by
                // reading the resulting Promise and calling
                // mark_as_handled (existing helper in algorithms.rs).
                let p = resolver.get_promise(scope);
                p.mark_as_handled();
            }
        }
        Ok(())
    }

    // ... read / releaseLock / cancel / closed (getter) — all become
    // #[v8_method] / #[v8_getter] callable on the box-installed instance.
}
```

Saved diff: ~70 lines of receiver-check boilerplate go away (the macro's
arg extraction + must-new + brand check covers it) plus the ~20-line
hand-rolled set_up + box install. Re-measured: see §7.2.

### 4.6 Codegen flow diagram (post-MAC-02)

Step numbers below match the §1.4 prose exactly. `gen_box_and_install_finalizer`
(line 1722–1747 of v8_class.rs) splits the post-user-body work into
4a (Box::into_raw), 4b (set_internal_field), 5 (with_guaranteed_finalizer),
5b (mem::forget). The new POST_INIT step is **6**.

```
                +-----------------------------+
                |   V8 calls ctor callback    |
                |   (from FunctionTemplate)   |
                +--------------+--------------+
                               |
                    +----------v----------+
                    | 1. must-new check   |
                    |    [throws if not]  |
                    +----------+----------+
                               |
                    +----------v----------+
                    | 2. JS-arg extract   |
                    |    (per-param)      |
                    +----------+----------+
                               |
                    +----------v----------+
                    | 2b. synthetic bind  |
                    |    scope, optional  |
                    |    `wrapper:`       |
                    +----------+----------+
                               |
                    +----------v----------+
                    | 3. user body        |
                    |    Self::ctor()     |
                    |    -> Self or Err   |
                    +----------+----------+
                               |
                       +-------+--------+
                       |    Err?        |
                       +-+------------+-+
                         |Yes        |No
                         v           v
                      throw        +---------------------+
                      return       | 4a. Box::into_raw   |
                                   |     (heap allocate) |
                                   +----------+----------+
                                              |
                                   +----------v----------+
                                   | 4b. ext + set_      |
                                   |     internal_field(0)|
                                   |     [box live in V8]|
                                   +----------+----------+
                                              |
                                   +----------v----------+
                                   | 5. weak finalizer   |
                                   |    register         |
                                   +----------+----------+
                                              |
                                   +----------v----------+
                                   | 5b. mem::forget weak|
                                   +----------+----------+
                                              |
                              +---------------v----------------+
                              | 6. POST_INIT (NEW)             |
                              |    if attribute set:           |
                              |    Self::hook(scope, this)     |
                              |     -> Result<(), OpError>     |
                              |                                |
                              |    Box live in field 0;        |
                              |    Brand check passes;         |
                              |    Methods on this work        |
                              +-----+----------------+---------+
                                    |                |
                                  Err               Ok
                                    v                v
                                  throw           return
                                  return          (V8 returns args.this()
                                                   per ECMA-262 §10.3.2)
                                  [box reclaimed by GC via weak finalizer
                                   on next sweep — see §4.4 for memory
                                   pressure analysis]
```

---

## 5. Risk and back-compat

### 5.1 `Self: Default` fallback constructor (line 1688)

`gen_default_constructor_callback` emits when there's no
`#[v8_constructor]`. Should `post_init` work with it?

**Decision: no.** `post_init` is a sibling of `#[v8_constructor]` in
attribute syntax (`#[v8_constructor(post_init = "...")]`). With no user
constructor, there's no attribute to read — and no obvious place to put
the attribute syntactically. If a class wants both Default-derived ctor
AND post-init, it can write a trivial `#[v8_constructor]` that calls
`<Self>::default()`:

```rust
#[v8_constructor(post_init = "after_install")]
fn new() -> Self { Self::default() }
```

This is one line of Rust. Cleaner than overloading the implicit-default
fallback.

### 5.2 `#[v8_constructor(must_new)]` interaction

The must-new check is the FIRST thing the callback does (line 1677). If
the receiver wasn't constructed with `new`, the callback throws and
returns BEFORE the user body, BEFORE the box install, BEFORE post_init.
So post_init only runs for valid constructions. **Confirmed.** No
interaction worth special-casing.

### 5.3 `#[v8_constructor(callable_no_new)]` interaction

`callable_no_new` (line 1569 `extract_callable_no_new`) is the opt-out
for `must_new`. When set, `Foo()` (no `new`) is allowed; `args.this()`
is the function itself in strict mode (or `globalThis` in sloppy).

The original v1 decision was "post_init runs by default in callable_no_new
mode". This is **wrong** — post_init writing private symbols on
`globalThis` is an information leak hazard, and writing internal-field
expectations into a function-typed `this` is incoherent (FunctionTemplates
don't allocate internal fields on the function object itself).

**Revised decision: post_init does NOT run when the call site is not a
construct call.** The macro emits an `is_construct_call()` guard
immediately before the post_init block:

```rust
if args.is_construct_call() {
    // post_init dispatch (as in §4.3)
}
```

This is invariant in `must_new` mode (post_init always runs because
`must_new` already guaranteed construct-call). In `callable_no_new` mode,
post_init only runs for `new Foo()` and skips `Foo()`. The `wrapper:`
synthetic in the user body, if present, still receives `args.this()`
unchanged (preserving WebSocket back-compat — but no class today combines
`callable_no_new` with `post_init`, so this is a precaution, not a live
codepath).

Document: "`callable_no_new` + `post_init`: the hook only fires for
`new` calls." Add a smoke test (Test #6, §6.1).

### 5.3a Cross-class private-symbol collisions (NEW in v3)

`Private::for_api(scope, Some(name_str))` interns its symbol by name
across the entire isolate. Two classes both writing
`slots::write_slot(scope, this, "stream", ...)` would share the same
Private and step on each other's storage.

Today's hand-rolls follow a convention: every priv-sym name is prefixed
with the class's "namespace" (e.g. `__zs_streams_default_reader_stream`
in the Reader; `__zs_streams_writable_writer_stream` in the Writer).
The convention is enforced by review, not by the type system.

**Recommendation for v1:** the implementing PR adds a Rust-level lint
(in `crates/runtime-macros/tests/priv_sym_lint.rs`) that walks the
`#[v8_class]` impl blocks via `syn` and asserts every `Private::for_api`
call's name argument is a string literal beginning with `__zs_`.

```rust
use syn::visit::Visit;

#[test]
fn priv_sym_names_are_class_prefixed() {
    let mut violations: Vec<String> = Vec::new();

    for entry in walkdir::WalkDir::new("../runtime/src/web")
        .into_iter()
        .filter_map(Result::ok)
        .filter(|e| e.path().extension().is_some_and(|x| x == "rs"))
    {
        let src = std::fs::read_to_string(entry.path()).unwrap();
        let parsed = match syn::parse_file(&src) {
            Ok(f) => f,
            Err(_) => continue, // skip non-parseable; rustc catches them separately
        };

        struct V<'a> {
            file: &'a std::path::Path,
            violations: &'a mut Vec<String>,
        }

        impl<'ast, 'a> Visit<'ast> for V<'a> {
            fn visit_expr_call(&mut self, call: &'ast syn::ExprCall) {
                // Match on path ending in `Private::for_api`.
                if let syn::Expr::Path(p) = &*call.func {
                    let last = p.path.segments.last().map(|s| s.ident.to_string());
                    let prev = p.path.segments.iter().rev().nth(1)
                        .map(|s| s.ident.to_string());
                    if last.as_deref() == Some("for_api")
                       && prev.as_deref() == Some("Private")
                    {
                        // call.args has 2: scope, Some(LIT). Inspect the
                        // 2nd arg's inner literal.
                        if let Some(syn::Expr::Call(some_call)) = call.args.iter().nth(1) {
                            // Some(LIT) is itself an ExprCall(Some, [LIT]).
                            if let Some(syn::Expr::Lit(lit_expr)) = some_call.args.first() {
                                if let syn::Lit::Str(s) = &lit_expr.lit {
                                    let v = s.value();
                                    if !v.starts_with("__zs_") {
                                        self.violations.push(format!(
                                            "{}: Private::for_api with non-__zs_ name {v:?}",
                                            self.file.display(),
                                        ));
                                    }
                                }
                            }
                        }
                    }
                }
                syn::visit::visit_expr_call(self, call);
            }
        }

        let mut v = V { file: entry.path(), violations: &mut violations };
        v.visit_file(&parsed);
    }

    assert!(
        violations.is_empty(),
        "Private::for_api calls with non-__zs_-prefixed names:\n{}",
        violations.join("\n"),
    );
}
```

This is **robust** to multi-line calls and code formatting (unlike
a grep-based approach). False-positive rate is zero (we're parsing
the AST). The walker does not catch dynamically-built names
(`Private::for_api(scope, Some(format!("foo_{}", x)))`) but no
class today does this; a follow-up MAC-NN can extend the walker
to flag dynamic names if a real consumer surfaces.

`walkdir` is present transitively in `Cargo.lock` (verified) but
NOT yet declared as a direct dev-dep on `runtime-macros`. The
implementing PR adds a one-line `walkdir = "2"` under
`[dev-dependencies]` in `crates/runtime-macros/Cargo.toml`. (Or use
`std::fs::read_dir` recursion — ~10 extra lines, no new dep — if
preferred. Either choice is fine; not load-bearing for the design.)

For new hooks: the proc-macro doc-string mandates a class-prefixed
naming convention (`__zs_<crate>_<class>_<purpose>`). Same as the
existing `__zs_same_object_<class>_<method>` pattern at line 1237
of v8_class.rs.

**Future work (deferred):** the macro could auto-prefix priv-sym names
with `stringify!(#class_ty)` so users can't get this wrong, but that's
a `slots::write_slot` API change with a wider blast radius than
post_init. Tracked in MAC-NN.

### 5.4 `#[v8_inherit]` interaction — worked AbortSignal example

A derived class's V8 FunctionTemplate inherits the base via
`__ctor_tmpl.inherit(__base_tmpl)` (line 856). At construction time, V8
walks the chain: it calls the **derived** class's ctor callback, which
sets up the derived instance — V8 does NOT automatically chain into the
base's ctor callback. (V8's `FunctionTemplate::inherit` chains the
prototype, not the constructor logic.)

So if both base AND derived classes have post_init, the derived's hook
runs; the base's does NOT. This is **the existing behaviour** for
constructors today (the macro-emitted ctor callback for the derived
class doesn't call the base's emitted ctor either).

#### Worked example: AbortSignal extends EventTarget

`crates/runtime/src/web/dom/abort_signal.rs:123` declares
`#[v8_class] #[v8_inherit(super::event_target::EventTarget)]`.
EventTarget today has `#[v8_constructor]` with no post_init needs;
its ctor stashes `EventTargetState::default()` (no priv-syms, no
resolvers).

If, in a future MAC-NN proposal, we added a post_init to EventTarget
(say, to lazily initialise an event-listener registry on a priv-sym
slot) — would AbortSignal's post_init need to chain into it?

**Decision in this proposal: no chaining.** The derived class is
responsible for invoking the base's setup if it wants that behaviour:

```rust
// Hypothetical: EventTarget has post_init for listener-registry.
#[v8_class] impl EventTargetState {
    #[v8_constructor(post_init = "after_install")]
    fn new() -> Self { Self::default() }

    /// `pub(crate)` so derived classes in sibling modules can chain.
    /// Hooks for classes that have no derived consumers MAY be
    /// default-private; the macro doesn't constrain visibility.
    pub(crate) fn after_install(
        scope: &mut v8::PinScope,
        this: v8::Local<v8::Object>,
    ) -> Result<(), OpError> {
        // ... priv-sym init for listener registry ...
        Ok(())
    }
}

// AbortSignal must opt in explicitly:
#[v8_class] #[v8_inherit(EventTargetState)]
impl AbortSignalState {
    #[v8_constructor(post_init = "after_install")]
    fn new(...) -> Result<Self, OpError> { ... }

    pub(crate) fn after_install(
        scope: &mut v8::PinScope,
        this: v8::Local<v8::Object>,
    ) -> Result<(), OpError> {
        // Chain into base EXPLICITLY. `scope` is reborrowed at the
        // call site (Rust's standard reborrow on `&mut T`); after
        // the call returns, `scope` is again usable in this fn body.
        // Verified: existing macro-emitted dispatch pattern does the
        // same thing in `gen_param_extractions`.
        EventTargetState::after_install(scope, this)?;
        // ... AbortSignal-specific priv-sym init ...
        Ok(())
    }
}
```

**Visibility rule:** if a class has any derived consumer that wants to
chain, the base's hook MUST be at least `pub(crate)` so the derived
class (typically in a sibling module) can call it. The macro itself
emits no visibility constraint — the hook is just a normal method on
the impl block, and rustc's standard module-privacy rules apply. The
proc-macro doc-string recommends `pub(crate)` for any hook that might
ever be chained.

**Why not auto-chain?** Three reasons:
1. **Symmetry with V8's existing ctor semantics.** Derived ctors don't
   auto-call base ctors today; auto-chaining post_init would create an
   asymmetry that confuses contributors.
2. **Composition flexibility.** A derived class might need to interleave
   "base setup → derived setup A → base setup tail → derived setup B".
   Auto-chaining forces a fixed "base before derived" order; explicit
   chaining lets the derived class pick.
3. **Today's reality.** Of the four streams classes, none use
   `#[v8_inherit]` (Reader, Writer, BYOBReader, TransformStream are
   peers, not subtypes of each other). EventTarget's post_init isn't
   on the v1 critical path; we don't ship the chaining infrastructure
   for a hypothetical future.

If multi-level chaining proves to be a recurring pattern, we can revisit
in MAC-NN with an explicit `super_post_init` annotation. v1 keeps the
contract simple: "your hook, your responsibility".

**v1 verification:** today, `EventTarget` has no post_init; AbortSignal
has no post_init either. The streams classes don't `#[v8_inherit]`
each other. So the chaining decision affects zero shipping classes.
We're locking in the policy preemptively.

### 5.5 Re-entrancy and aliasing — deep-dive (Option C selected for hook signature)

The original v1 sketch passed `&Self` to the hook. v2 selects the
no-`&Self` form (the one §5.5 of v1 reached late in the doc). Recap of
the four mitigation options that were considered:

A. **Document forbidden usage.** Pass `&Self`; tell users not to expose
   it to JS that could reach `&mut self` methods. Brittle; relies on
   user discipline. **Rejected.**

B. **Use `*const Self` instead of `&Self`.** Pass the raw pointer; the
   user dereferences inside an unsafe block. Less ergonomic than the
   existing `with_state` idiom that the streams classes already use.
   **Rejected** (same effective surface as A, with worse ergonomics).

C. **Don't materialise a borrow at all.** Pass `(scope, this)` only;
   the user calls `with_state(scope, this, |s| ...)` to access the box
   state through the External in field 0. Matches the existing
   helper pattern. The macro doesn't emit any borrow at all — the user
   does, through a closure that's opaque to the macro. **Selected.**

D. **Re-entry guard at post_init level.** Insert a thread-local
   "post-init in flight" set keyed by the box address; before any
   `&mut self` method materialises `&mut Self`, check this set. Adds
   complexity to every method callback; probably overkill given C is
   strictly safer and has no runtime cost. **Rejected.**

**Why C is strictly safer than A or B:** the macro emits no `&Self`
borrow in post_init. A reentrant `&mut self` method materialises
`&mut Self` from the External; nothing else has a borrow into the same
allocation; aliasing is impossible at the macro layer. The user's
`with_state` closure takes `&Self` — that closure runs to completion
before any reentrant call could fire (closures don't yield), so its
`&Self` borrow ends before any external code can run. Reentrancy from
inside a `with_state` closure is a separate concern that the user
controls (don't call into JS while holding `with_state`'s `&Self`); the
existing streams classes already follow this discipline.

The macro signature is:

```rust
fn after_install(
    scope: &mut v8::PinScope,
    this: v8::Local<v8::Object>,
) -> Result<(), OpError>;
```

The user reads box state via the existing `with_state` helper they've
already written for their methods. The macro doesn't emit any unsafe
borrow at all. **Re-entry from inside post_init goes through the
existing method codegen's brand check + reentry guard — no new holes.**

### 5.6 What if the user's `Self::ctor` returns `Self` (not `Result<Self, _>`)?

`gen_constructor_callback` (line 1639) has both arms. The post_init
emission slots in identically — after `make_instance` and `store`,
regardless of whether `make_instance` is the `Result` arm or plain
arm. **Verified by reading the code; covered by Test #8 in §6.1.**

### 5.7 Compile-fail surface

- `#[v8_constructor(post_init = "missing_fn")]` where `missing_fn`
  doesn't exist on the impl block: rustc emits "no function named
  `missing_fn` in the implementation" at the call site we generate.
  Decent diagnostic, span lands on the auto-generated callback.
- Wrong signature: rustc emits "this function takes 1 argument but 2
  were supplied" or "expected `Result<(), OpError>` found `()`" etc.
  Per-class spans, not on the attribute itself — slightly worse than
  ideal. **Improvement (v1 → v2):** the proc-macro emits a `#[doc =
  "..."]` attribute on the generated callback with the expected hook
  signature inline:

  ```rust
  // Emitted on every constructor callback that uses post_init:
  #[doc = concat!(
      "Auto-generated constructor callback for `", stringify!(#class_ty), "`.\n",
      "\n",
      "post_init hook expected signature:\n",
      "    fn ", stringify!(#hook_ident), "(\n",
      "        scope: &mut v8::PinScope,\n",
      "        this: v8::Local<v8::Object>,\n",
      "    ) -> Result<(), OpError>\n",
      "\n",
      "See docs/reference/plugin-system.md#post-init-hook.",
  )]
  pub(crate) fn #callback_ident(...) { ... }
  ```

  When rustc reports "no function named X" the doc comment shows up
  on the generated symbol; rust-analyzer surfaces it on hover. Better
  than nothing, not as good as a custom error message at the attribute
  span (which would require a more invasive macro rewrite tracked in
  MAC-NN).
- Non-string-literal value (`#[v8_constructor(post_init = ident)]`):
  `extract_post_init` (§4.2) returns `Err` from the parser path; the
  macro propagates via `e.to_compile_error()`. The error message is
  `"post_init must be a string literal naming a function on this
  impl, e.g. post_init = \"after_install\""` with the span of the
  offending value. This is **strict by design** — silent no-op would
  be a debugging nightmare given post_init is semantically load-bearing.
- Non-identifier string (e.g. `post_init = "1bad"`): macro emits
  `"post_init = "1bad" is not a valid Rust identifier"` with the span
  of the string literal.

### 5.8 Stack-trace and error propagation (NEW in v2)

When post_init returns `Err(OpError)`, the macro's emitted block calls
`scope.throw_exception(...)` with a fresh V8 Exception built from the
`OpError`'s message and kind. V8's stack-trace capture point is:

- The exception's `message` is what the user wrote.
- The exception's stack trace is captured at `Exception::*` construction
  time; with V8's default settings, the trace shows JS frames only,
  starting at the `new Foo()` call site, then the macro-emitted
  constructor callback (anonymous), then native frames are elided.

The user's `after_install` Rust source location does NOT appear in the
JS stack trace (V8 has no concept of Rust frames). For Rust-side
debugging, the user can add `tracing::error!(target = "v8_class",
class = stringify!(#class_ty), ...)` in their `after_install` body —
the proc-macro doc-string recommends this pattern.

If a hook panics (Rust panic, not OpError), the existing macro convention
applies: panic across V8 C++ frames is undefined / SIGABRT on Linux. The
`after_install` body MUST NOT panic; map all error paths to `OpError`.
The proc-macro doc-string makes this explicit.

### 5.9 OOM during post_init (NEW in v2)

`PromiseResolver::new(scope)` returns `Option<Local<PromiseResolver>>`.
None means isolate termination or OOM. `v8::String::new(scope, msg)`
similarly. Hooks MUST handle these:

```rust
let resolver = v8::PromiseResolver::new(scope)
    .ok_or_else(|| OpError::error("PromiseResolver::new failed (likely OOM)"))?;
```

The proc-macro doc-string includes a "OOM checklist" listing the V8
APIs that return `Option`/`Maybe` and require `.ok_or_else(OpError::...)`
wrapping. The Reader migration sketch in §4.5 demonstrates the pattern.

Under platform constraints (multi-tenant, V8 isolate per app, ~256 MB
heap), OOM during construction is plausible at scale. Surfacing it as
an `Error` (rather than panicking) is consistent with the rest of the
runtime's error handling.

### 5.10 `#[v8_async_method]` interaction (NEW in v2)

Constructors are sync (§1.6). But post_init can call into a
`#[v8_async_method]` (which queues a microtask via the runtime's async
event loop). Two questions:

1. **Does the microtask see the wrapper as fully constructed?** Yes —
   by the time the microtask runs, the constructor callback has
   returned, V8 has returned `args.this()` to the JS caller, and the
   wrapper is fully visible. The microtask reads box state via
   `with_state` like any other method.

2. **Does the microtask's success or failure affect the constructor's
   completion?** **No.** The constructor returns synchronously; the
   microtask completes (or fails) independently. If the microtask
   throws, the rejection propagates through whatever Promise the
   caller is awaiting — but the constructor has already returned a
   live wrapper. This matches WebIDL's "set up the object, then
   schedule the start algorithm in parallel" model (e.g. Streams §5.4).

3. **Fire-and-forget microtask rejections.** If no caller awaits the
   Promise that the microtask rejects, V8's promise-rejection callback
   (set via `set_promise_reject_callback` in
   `crates/runtime/src/core/init.rs`) fires with `kPromiseRejectWithNoHandler`.
   The runtime's existing handler logs an unhandled-rejection diagnostic —
   same surface as any other async-method-driven unhandled rejection.
   **No new behavior** introduced by post_init. *(Verification owed by
   implementer: read the actual `set_promise_reject_callback` registration
   and confirm metric increment behavior; if the runtime doesn't increment
   today, that's a separate gap, not a post_init concern.)*

No special handling needed. Document the pattern in the proc-macro
doc-string with a "post_init can dispatch async work" note. Test #12
(§6.1) covers the happy path.

### 5.11 Telemetry (NEW in v2)

The runtime emits class-level construction metrics today (see
`crates/runtime/src/metrics/`). post_init failures should increment
a new counter:

```
runtime.v8_class.post_init_errors{class="Foo",kind="TypeError|RangeError|..."}
```

Wired by emitting a `tracing::error!(target = "v8_class.post_init",
class = stringify!(#class_ty), kind = ?__err.kind, "post_init failed")`
at the throw site in §4.3. The metrics layer subscribes to this target
and increments per-class, per-kind. **Decision:** ship in v1; no
infrastructure burden — the macros already emit `tracing::error!` in
the `make_instance` Err arm (line 1656). Mirror it.

### 5.12 Wire-format / breaking change risk

None. The attribute is opt-in; existing classes (zero `post_init`
attributes today) emit identical bytecode. The runtime ABI doesn't
change — the constructor callback's signature is unchanged from V8's
perspective. The proc-macro version bump (in `runtime-macros/Cargo.toml`)
is patch-level: no public macro API changes, only a new attribute key
recognised inside `#[v8_constructor(...)]`.

### 5.13 Documentation deliverable (NEW in v2)

The implementing PR MUST update `docs/reference/plugin-system.md` with:
- A new "Constructor post-init hook" subsection.
- The `(scope, this) -> Result<(), OpError>` signature with explanation.
- A worked example (the Reader migration from §4.5, condensed).
- A "common pitfalls" callout: don't call into JS while inside a
  `with_state` closure; map V8 OOM returns to OpError;
  no panicking; class-prefix priv-sym names (per §5.3a).

**Enforcement:** the implementing PR's description includes a checklist:

```
- [ ] docs/reference/plugin-system.md updated with post_init section
- [ ] CHANGELOG.md entry under runtime-macros
- [ ] trybuild fixtures land under crates/runtime-macros/tests/compile_fail/
- [ ] CI grep step for un-prefixed priv-sym names (§5.3a)
- [ ] Tests #1–12 plus #5b and #11b land in crates/runtime/tests/v8_post_init_smoke.rs (14 cases total)
- [ ] crates/runtime/benches/v8_post_init.rs ships
```

Review must block on each box being checked. The repo's PR template
lives at `.github/PULL_REQUEST.md` (verified: file exists at workspace
root). Note: GitHub's auto-population of the PR description body uses
the canonical filename `PULL_REQUEST_TEMPLATE.md`; the project's
chosen name `PULL_REQUEST.md` requires manually copy-pasting the
template into the PR body. Either way,
the implementing PR adds a one-line addendum:

```markdown
- [ ] Macro changes? Update `docs/reference/plugin-system.md`.
```

If GitHub's auto-population is desired, a separate housekeeping PR
can rename to the canonical filename — out of scope for MAC-02.

### 5.14 V8 startup-snapshot interaction (NEW in v3)

The runtime uses V8 startup snapshots (`crates/runtime/src/core/init.rs`
captures pre-installed classes' FunctionTemplates into a snapshot blob
loaded on each isolate's startup). post_init affects the **constructor
callback's emitted body** but NOT the FunctionTemplate's shape (the
template is created with `FunctionTemplate::new(scope,
constructor_callback)` — the function pointer is what gets snapshotted,
not the body).

**Invariant:** the function pointer to `__Foo_constructor_callback`
must be stable across all snapshot creators and consumers of a given
binary. Within a single binary build, this holds (the symbol is
emitted in the same crate). Cross-binary snapshot reuse (e.g.
distributing a snapshot built with binary A to a fleet running binary
B) would break — but the runtime doesn't do this; snapshots are
re-built per binary version.

If a future deployment scheme distributes pre-built snapshots, the
post_init function pointer must be part of the binary-version-pin
hash. **Documented for future awareness.** *(Verification owed by
implementer: confirm `crates/runtime/src/core/init.rs`'s snapshot-creation
path doesn't bake in any per-build-version assumption that would be
incompatible with the FunctionTemplate pointer-stability invariant.)*

### 5.15 Compile-time perf (NEW in v3)

`extract_post_init` runs once per `#[v8_constructor]` invocation. The
parser walks at most a few dozen tokens (typical attr arg list).
Negligible: <1ms per class, <100ms cumulative across the full
runtime crate. No measurable build-time impact.

### 5.16 Runtime perf (NEW in v3)

The post_init dispatch costs: one indirect call (Rust → user fn) +
one Result match + one set of `OpError → Exception` mapping (only on
error). Methodology:

- Indirect call (cold-cached function pointer): ~3 ns on modern x86-64
  (typical Skylake / Zen3 latency for a non-speculatively-predicted
  indirect branch).
- `Result<(), OpError>` match on Ok: ~1 ns (single tag compare on
  the discriminant; LLVM elides the no-payload branch).
- Total happy-path: ~4 ns per construction. Round to ~5 ns as a
  rough budget. **Scope:** this is the post_init code path's call
  dispatch overhead only. The user's `after_install` body work is
  class-specific and not part of the macro's per-class budget.

The error path adds a `v8::Exception::*` allocation (~50 ns) but only
fires on failure.

For an existing `zerobench` benchmark that exercises stream
construction (closest is `crates/runtime/benches/streams.rs`'s
`bench_pipe_through_throughput` which incidentally constructs ~10k
Readers/sec inside the harness), 10k × 5 ns = 50 µs/sec of overhead —
well below the 1% "don't worry about it" threshold.

**Regression benchmark commitment.** The implementing PR adds a
dedicated `crates/runtime/benches/v8_post_init.rs` microbenchmark
(criterion-based, matching the existing `crates/runtime/benches/`
convention; distinct from the user-facing `zerobench` HTTP/SSE/WS
tool) that:

1. Constructs 1M `DefaultReader` instances with `post_init` opted in.
2. Reports ns/construction (target: ≤30 ns including box install).

Plus, the streams migration PR (per §7.1 step 2) re-runs
`bench_pipe_through_throughput` pre/post and asserts ≤2% throughput
drop vs the hand-rolled baseline. If larger, the migration PR blocks
and we investigate (typical suspects: extra allocation in the new
flow path, accidental `format!` in error message).

### 5.17 Lint / CI commitments (NEW in v3)

The implementing PR adds two checks:

1. **Un-prefixed priv-sym names** (§5.3a). The syn-based test walker
   described in §5.3a runs as part of `cargo test -p zeroship-runtime-macros`.
   Failure (a `Private::for_api` call without `__zs_` prefix in its name
   argument) blocks the test suite.

2. **Hooks declared but unused.** A custom proc-macro lint (or a
   `cargo-clippy`-driven grep) for: methods named in `#[v8_constructor(post_init = "X")]`
   that exist in the impl block but no other reference exists.
   Catches typos in the attribute string. **Deferred to MAC-NN if
   not trivially expressible** — rustc's "no function named X" already
   catches the inverse case.

---

## 6. Testing strategy

New test file: `crates/runtime/tests/v8_post_init_smoke.rs`. Style
matches `v8_must_new_smoke.rs` and `v8_brand_check_smoke.rs` — local
`run_in_v8` harness, isolated test classes per module.

### 6.1 Test cases

| # | Test | What it proves |
|---|---|---|
| 1 | `post_init writes priv-sym, method reads back` | Basic happy path: `after_install` writes a private symbol; a `#[v8_method]` reads it. Asserts the priv-sym is set after `new Foo()` returns. |
| 2 | `post_init allocates PromiseResolver, stashes via with_state` | Streams use case in microcosm: `new` returns `Self { closed_resolver: RefCell }`; `after_install` reads via `with_state`, allocates the resolver pair, mutates state through interior mutability. JS asserts `inst.closed instanceof Promise`. |
| 3 | `post_init returns Err — constructor throws, no leak` | `after_install` returns `Err(OpError::type_error("post_init failed"))`; JS tries `new Foo()` and catches a TypeError with the right message. The weak finalizer drops the Box (verify by counting drops via a `static AtomicUsize` incremented in `Drop`). Asserts the count rises after a forced GC (`scope.request_garbage_collection_for_testing()`). |
| 4 | `post_init in subclass via #[v8_inherit]` | Define `Base { #[v8_constructor(post_init = "base_post")] new(){} }` and `Derived: Base { #[v8_constructor(post_init = "derived_post")] new(){} }`. Verify ONLY `derived_post` runs (per §5.4 decision). The derived's hook can call `Base::base_post(scope, this)` explicitly; verify that pattern too. |
| 5 | `post_init can call brand-checked &self methods on this` | The hook calls a `&self` `#[v8_method]` on the wrapper via `Reflect.apply` (small JS shim run via `Local<Function>::call` from the hook). Asserts the method's brand check + box recovery work. |
| 5b | `post_init can call &mut self methods on this` | The hook calls a `&mut self` `#[v8_method]` on the wrapper. Verifies: (1) the per-method re-entry guard sees zero prior entries and admits the call; (2) the mutation persists (state read post-construction reflects the mutation); (3) no UB (the macro emits no `&Self` borrow in post_init's prologue, so no aliasing). |
| 6 | `post_init coexists with #[v8_constructor(must_new)]` | `Foo()` (no new) throws the must-new TypeError BEFORE post_init runs. Verify post_init's side-effect (a `Cell<bool>` flip in box state) is NOT observed. |
| 7 | `post_init calls with_state on this` (TransformStream-shape) | Define `Foo` with a state field that `after_install` reads via `with_state(scope, this, |s| ...)` — verifies the External in field 0 is non-null AND the cast yields the correct Box. Without this test, the TransformStream migration ships on faith. |
| 8 | `post_init with plain-Self ctor (no Result)` | A class whose `#[v8_constructor]` returns `Self` (not `Result<Self, _>`) AND has post_init. Cover the §5.6 verified-but-untested arm. |
| 9 | `post_init + callable_no_new — hook skipped on bare call` | Class with both attributes set. JS calls `new Foo()` → hook fires; JS calls `Foo()` → hook does NOT fire. Verifies the §5.3 guard. |
| 10 | `post_init OOM path` | Stub `PromiseResolver::new` to return None (via test-only feature flag in the runtime); hook returns `Err(OpError::error(...))`; constructor throws Error; no panic, no SIGABRT. |
| 11 | `post_init throw — JS stack trace shape` | Capture a thrown error's `e.stack` from JS via `try { new Foo() } catch (e) { e.stack }`. Assert: stack contains the JS-side `new Foo()` call site; does NOT contain Rust frame names (V8 has no concept of Rust frames, per §5.8). Locks in the documented behavior; if a future V8 release changes stack capture, this test catches it. |
| 11b | `OpError → JS Exception message hygiene (paired smoke)` | Two paired fixtures: (i) a class whose hook returns `OpError::error(format!("ptr={:p}", &state))`; the test asserts `e.message =~ /0x/` matches in this fixture (the bad pattern IS detected). (ii) a class with a hand-written safe message; the test asserts the same regex does NOT match (no false positive). **NOT applied to streams classes' real error paths** — those have their own message conventions; gating them on this test would create migration friction. The pair documents the hygiene contract for new authors. |
| 12 | `post_init can dispatch async work` | Hook calls a `#[v8_async_method]` on `this`; the async method queues a microtask via the runtime's compio bridge. Assert: the constructor returns synchronously (test JS observes the wrapper); the microtask later completes and its side effect is visible. Verifies §5.10. |

### 6.2 Compile-fail tests

Use `trybuild` if it's already in the workspace; else add the dependency
(it's a single line in `runtime-macros/Cargo.toml`'s `[dev-dependencies]`).
Test cases live under `crates/runtime-macros/tests/compile_fail/`:

```text
post_init_missing_fn.rs       // no_such_fn doesn't exist on impl
post_init_bad_ident.rs        // post_init = bad_ident (not a string lit)
post_init_invalid_string.rs   // post_init = "1bad" (not a valid ident)
post_init_wrong_sig.rs        // hook takes 0 args (rustc reports)
post_init_wrong_return.rs     // hook returns () instead of Result
```

Each `.rs` file has a paired `.stderr` snapshot; `trybuild` asserts
the compile error matches. Span quality verified by inspection.

### 6.3 Regression coverage

After this lands and the streams classes migrate (separate PR):

- Existing WPT suites for streams MUST stay green:
  `streams/readable-streams/`, `streams/writable-streams/`,
  `streams/transform-streams/`. The migration changes implementation,
  not behaviour; WPT is the spec-conformance backstop.
- Local smoke tests for each class continue to pass:
  `crates/runtime/tests/streams.rs`, `crates/runtime/tests/streams_native.rs`.
- Memory: the existing `streams/budget` tests cover the budget guard
  drop path. Make sure `after_install` errors don't double-drop the
  guard (the guard is on `Self`; the box drops via finalizer once;
  guard's `Drop` runs once. **Confirmed by reading drop impls.**).
- `runtime.v8_class.post_init_errors` counter (per §5.11) becomes a
  signal we can alert on in production.

### 6.4 Stress / fuzz

Not motivated for this change. Re-entrancy stress is covered by
`v8_reentrancy_smoke.rs` for methods; post_init's no-`&Self` signature
(per §5.5 Option C) means there's no new aliasing surface to fuzz.

If a future class' post_init body becomes complex enough that
construction failure rate matters, MAC-NN can add a per-class fuzz
target; v1 keeps the test surface bounded.

---

## 7. Migration path

### 7.1 Order of operations

1. **Land MAC-02 macro change alone.** New attribute, new test file,
   new docs section, no consumer migration. Existing classes unaffected
   (nothing emits the new attribute).
2. **Migrate Reader** (`readable_default_reader.rs`). Smallest blast
   radius; the SetUp helper is shared with `acquireReader` so the
   refactor splits its reusable core into a free function and the
   hook calls it. Streams WPT must stay green.
3. **Migrate BYOBReader** (`readable_byob_reader.rs`). Same shape;
   add the BYOB tag write to `after_install`.
4. **Migrate Writer** (`writable_writer.rs`). The 4-way state branch
   on `WSState` becomes match-arm logic in `after_install`.
5. **Migrate TransformStream** (`transform.rs`). The acid test:
   `set_up_transform_stream_default_controller_from_transformer` runs
   from inside `after_install`. Confirm it can synchronously call
   into `with_ts_state` (the TransformStream-specific wrapper around
   `with_state` — class-specific naming convention; Reader uses
   `with_state`, BYOB uses `with_byob_state`, Writer `with_writer_state`,
   TS `with_ts_state`) without seeing an empty External (covered by
   Test #7 above).
6. **Audit Request/Response migration plan** (separate proposal):
   confirm the SameObject-cached `headers` getter slot can be
   pre-minted in `after_install` too.

   **Spike result (added v3):** Request needs `Headers::new(scope,
   /* init */)` to mint a Headers JS object, plus a write to the
   `__zs_same_object_Request_headers` private symbol on `this`
   (matching the pattern at line 1237 of v8_class.rs). The mint can
   happen inside `after_install` because: (a) `Headers` is itself
   a `#[v8_class]` in the same isolate, with its own
   FunctionTemplate already installed; (b) the priv-sym write goes
   through `set_private` which doesn't brand-check on `this`; (c) no
   reentrant call into `this`'s methods is needed during the mint.
   **Conclusion: post_init covers Request's needs without extension.**
   If the actual migration surfaces an unforeseen need, this proposal
   does NOT block it — MAC-NN can extend post_init.

Each step is a separate PR; each gates on streams WPT staying green.

### 7.2 Per-class diff estimates (LoC) — re-measured 2026-05-04

Re-measured with `wc -l`. The v1 estimates were inflated 2× or more.

The "After (est.)" column is computed by:

- **N** = lines removed from `is_construct_call` + must-new throwing
  + arg-typecheck-and-throw boilerplate (per file, ~25 lines).
- **M** = lines removed from per-method brand-check + External
  recovery + `with_state` wrapper that the macro now generates. The
  Reader has 6 methods, BYOBReader 7, Writer 8, TS 5; per method
  saves ~10 lines.
- **K** = lines removed from the hand-rolled `set_up_*` helpers
  (Box::into_raw + ext + set_internal_field + with_guaranteed_finalizer
  + mem::forget, ~12 lines each), minus the new `after_install` body
  (~25 lines for Reader, more for others).
- **Total estimated reduction = N + (M × method-count) + K_net**.
- After = Today − (N + M-net + K-net).

| Class | Today (LOC) | N | M-net | K-net | After (est.) | Net |
|---|---:|---:|---:|---:|---:|---:|
| Reader | 742 | 25 | 60 | 0 (helper-net wash) | 657 | -85 |
| BYOBReader | 894 | 25 | 70 | 5 (BYOB tag write fits in hook) | 794 | -100 |
| Writer | 795 | 25 | 80 | -10 (4-way state branch is verbose) | 700 | -95 |
| TransformStream | 905 | 25 | 50 | 30 (controller-setup helper inlined) | 800 | -105 |
| **Total** | **3336** | — | — | — | **2951** | **-385** |

The numbers are **lower** than v1's claim of ~1300 LoC saved — the
honest answer.

**Honest framing:** the LoC argument is secondary. The **real**
benefit is unblocking TransformStream's migration to `#[v8_class]`,
which removes a maintenance hazard (every change to v8_class needs
manual ports to TransformStream's hand-roll). The per-class LoC
savings are a nice-to-have, not the primary justification.

### 7.3 Spec deviations the hand-rolls preserve

I checked each constructor against the spec sections. The only
deviation worth flagging:

- **`writable_writer.rs:282-283`**: the hand-roll writes `writer.[[stream]]
  = stream` and `stream.[[writer]] = writer` in a specific order
  (writer first, stream second). The spec's
  `SetUpWritableStreamDefaultWriter` step 3.a says "Set
  writer.[[stream]] to stream", step 3.b "Set stream.[[writer]] to
  writer" — same order. **Migrating to `after_install` preserves this
  order trivially.**

- **`readable_default_reader.rs:283-296` weak finalizer registration**:
  the hand-roll calls `set_internal_field` BEFORE `with_guaranteed_finalizer`
  registration. The macro's `gen_box_and_install_finalizer` does the
  same order. **No drift.**

- **TransformStream's start algorithm**: §5.4 says the `start`
  algorithm runs "in parallel" (i.e. asynchronously); the hand-roll
  invokes it via the controller-setup path which schedules a microtask.
  `after_install` can call `set_up_transform_stream_default_controller_…`
  identically, and the microtask scheduling is unaffected. **No drift.**

No spec deviations identified that the hand-rolls preserve and the
macro would lose. Migration is a structural refactor.

### 7.4 Roll-back plan

Each migration PR ships in a single commit and tags a revert candidate.

**Revert window: 3 days** post-merge for `git revert <sha>` to be
the default rollback path. Rationale: streams WPT runs nightly (per
`tests/e2e_platform.sh` schedule); 3 days = 1 nightly + 2 buffer days
for triage. After 3 days, regressions surface against newer commits
on top of the migration; reverts become harder. The fix path beyond
3 days is forward (patch the macro or the migration), not backward
(cherry-pick the hand-roll back from history).

The macro change itself is opt-in (no existing class uses it), so
reverting MAC-02 is also clean — the attribute parsing becomes dead
code, no consumer breaks.

**Not** considered: a `cfg(feature = "legacy-stream-ctors")` dual-path
build. Cost (two implementations to maintain through the deprecation
window) outweighs benefit (the migration is a single commit; if it
breaks, revert is a single commit too).

---

## Open questions

(Each item is marked **(decided)** or **(open)**; (decided) items are
locked-in design choices kept here for the rationale trail; (open)
items are deferred to MAC-NN.)

1. **(decided) Should we name the attribute `post_init` or something more
   spec-y like `set_up`?** Streams and WebIDL spec algorithms are
   uniformly named `SetUp…` (`SetUpReadableStreamDefaultReader`,
   `SetUpWritableStreamDefaultWriter`, …). `set_up = "fn_name"`
   matches the spec lexicon. `post_init` is more
   developer-implementation-y.

   **Decision: `post_init`.** Cited references:
   - Cloudflare workerd's `JSG_RESOURCE_TYPE` calls the equivalent
     `jsgInitFromBuilder` — explicitly post-construction phase.
   - Mozilla's WebIDL backend uses `JS::Wrap` followed by an
     `OnConstruct` hook with the same semantics.
   - Node.js's N-API `napi_wrap` + post-construct user callback follows
     the same "wrap, then init" sequencing.

   "post_init" describes when it runs (after the box install); "set_up"
   would conflate with WebIDL's `[[SetUp]]` algorithms which include
   pre-construct steps too. The non-streams consumers (Request) aren't
   spec'd as "set up" either.

2. **(decided)** Should `after_install`'s name be the default if the attribute is
   bare `#[v8_constructor(post_init)]` (no value)?** Probably yes;
   matches `#[v8_iterable]` (no value form is "use defaults"). But
   then `after_install` is a magic name.

   **Decision: require the explicit `post_init = "fn_name"` form, no
   default.** The string is clearer at the call site; future readers
   don't have to know the magic default. Cost is one extra string per
   class — acceptable.

3. **(decided)** Re-entrancy guard for post_init itself. If post_init calls a JS
   function that re-enters the same instance's CONSTRUCTOR (via
   `Reflect.construct(Foo, [])` or `new Foo()`), the second
   construction is a separate invocation with its own `args.this()`
   — different wrapper, different box. Not aliased. **No new guard
   needed.** (Confirmed by walking through V8's [[Construct]] flow.)

4. **(decided)** Could post_init be the right place to install
   `[v8_inherit_intrinsic]`-style chained prototypes?** Today
   `inherit_intrinsic` runs at install time (line 810). It walks
   `[][Symbol.iterator]()` to find `%Iterator.prototype%`. Could
   move to per-instance post_init — but it's strictly slower:
   `Script::compile + run` of a literal JS expression is ~50µs on a
   warm isolate (measured via `crates/runtime/benches/`). At 100k
   constructions/sec that's 5 sec/sec of pure compile/run cost —
   catastrophic. Install-time runs once per isolate (~one isolate
   per app). **No.** Keep in install.

5. **(decided for v1; revisit-able in MAC-NN)** Does `after_install` need access to the constructor's args?
   Yes for some classes (TransformStream needs the `transformer`
   object). The proposal as written has `after_install` take only
   `(scope, this)`. If the class needs ctor args, they must be
   stashed on `Self` first (as Globals or as priv-sym slots written
   from the user body). For TransformStream this means:

   ```rust
   pub struct TransformStream {
       pending_transformer: RefCell<Option<v8::Global<v8::Value>>>,
       // ... other state ...
   }
   #[v8_constructor(post_init = "after_install")]
   fn new(scope, transformer: v8::Local<v8::Value>, ...) -> Self {
       Self {
           pending_transformer: RefCell::new(Some(v8::Global::new(scope, transformer))),
           // ...
       }
   }
   fn after_install(scope, this) -> Result<(), OpError> {
       let t = with_state(scope, this, |s| s.pending_transformer.borrow_mut().take())
           .flatten()
           .ok_or_else(|| OpError::error("after_install: missing pending_transformer"))?;
       let t = v8::Local::new(scope, &t);
       // ... use t ...
       Ok(())
   }
   ```

   Slightly awkward (one Global allocation, one `take()`).

   **Alternative considered:** plumb constructor args into `after_install`
   by re-extracting from `args.this()`'s priv-sym slots written in the
   user body. Cleaner for some shapes but couples post_init to a
   specific args-stashing convention; today's `with_state` idiom is
   already universal across the four streams classes that motivate
   this proposal, so reusing it minimises new patterns and gives
   users a single mental model. (Caveat: Request/Response don't yet
   have a `with_state` helper. If their migration (likely the next
   post_init consumer per §1.3) adopts post_init, it will likely add
   `Request::with_state` matching the streams pattern. Until then,
   the idiom is strictly current-to-streams.)

   **Decision: v1 ships with `(scope, this)` only.** The Global+take
   pattern is one extra allocation per construction (`v8::Global::new`
   is essentially a heap-allocated pointer; ~24 bytes). On a worker
   that constructs ~100 streams/sec, that's ~2.4 KB/sec of churn — far
   below the 256 MB heap budget. Acceptable. If consumers complain,
   MAC-NN can extend with `args:` synthetic.

6. **(decided — user responsibility)** What about `after_install` failing AFTER it's done partial
   side-effects on JS-visible state?** The user's responsibility:
   any state written before the Err must be cleaned up. The macro
   can't help here — same as today's hand-rolls. The proc-macro
   doc-string includes a "partial cleanup is your problem" note.
   For the streams migrations specifically, the existing hand-rolls
   don't have partial-cleanup logic either (they treat construction
   as all-or-nothing); the migration preserves this.

7. **(decided — no)** Should `after_install` get the same brand check as
   `#[v8_method]`s do?** It's invoked from the macro-emitted
   constructor callback, not from JS via the prototype chain. The
   brand check would always pass (we just installed the prototype +
   field 0). **No.** Brand check is for JS-side reflection; not
   relevant here.

8. **(decided)** Versioning policy. The proc-macro's public surface (attribute
   names, attribute semantics) is stable across patch versions. Adding
   `post_init` is a **minor** version bump (semver: new feature,
   backward-compatible). Removing or renaming an existing attribute
   would be a **major** bump. The codegen-internal changes (e.g. the
   exact tokens emitted, error message wording) are not part of the
   public surface — patch bumps may change them; consumers cargo-update
   freely without breakage. Documented in `runtime-macros/CHANGELOG.md`.

---

## Process notes (review-only — strip before landing)

- **Changelog at end of doc.** The HTML-commented changelog block
  below is review-only. The implementing PR strips it before landing
  the doc to `docs/proposals/`. Rationale: the proposal's final landed
  form is a stable record of the design; the round-by-round evolution
  is review history that belongs in PR comments, not in-tree.
- **Verifications owed by implementer.** The proposal flags two cited
  facts marked "verification owed by implementer": (1) the runtime's
  unhandled-rejection metric behavior in §5.10, (2) the snapshot path's
  per-build assumptions in §5.14. Both are minor (failure modes don't
  block the migration) but should be confirmed during code-up rather
  than deferred to runtime.

---

<!-- v1 → v2 changelog
- §1.1 / §1.2: removed the v1 section that argued Reader's "[[closedPromise]] priv-sym must be written before the box is installed — fragile smell". With Option B, no priv-sym is written before box install; the smell argument is moot.
- §1.5: NEW — WebSocket vs streams comparison table.
- §1.6: added multi-hook ordering note (deferred to MAC-NN with explicit ordering rule).
- §2.B: rewrote to match §5.5's selected (scope, this)-only signature. Removed all `&Self` references from the recommendation; consolidated reentrancy reasoning here.
- §3.1 / §3.6: ECMA-262 §10.3.2 citation for the "V8 returns args.this() when callback writes nothing" claim.
- §3.2: rewrote to match the no-`&Self` signature; removed the `&Self`-aliasing UB discussion.
- §3.4: added explicit `Foo::install` precondition statement; rewrote the brand-check argument with the no-`&Self` signature.
- §3.5: cited V8 source path (JSObject::SetEmbedderField, WRITE_FIELD).
- §3.6: NEW — ECMA-262 [[Construct]] return semantics.
- §4.3: corrected the OpErrorKind variants (5, not 6 — no JsValue) by reading crates/runtime/src/core/state.rs:32.
- §4.4: weakened the "no leak" claim to match V8's actual GC semantics; added memory-pressure analysis.
- §4.5: rewrote the Reader migration sketch with the no-`&Self` signature (uses `with_state` instead of `&Self` borrow); fixed the v1 "wrong direction" comment + missing `pending_stream` plumbing.
- §4.6: split steps 4 (Box::into_raw), 6 (set_internal_field), 7 (weak finalizer) per `gen_box_and_install_finalizer` actual ordering. Updated to use the no-`&Self` signature.
- §5.3: REVERSED v1's "post_init runs in callable_no_new mode" decision. Now: post_init only runs for `is_construct_call()` true.
- §5.4: NEW — worked AbortSignal: EventTarget example; explicit "no auto-chain" rationale.
- §5.5: kept the Option C selection but moved the analysis up so §2.B aligns; removed the `&Self` mitigation discussion that no longer applies.
- §5.7: emit `compile_error!` with explicit message text via `syn::Error::to_compile_error()`.
- §5.8: NEW — stack-trace and error propagation.
- §5.9: NEW — OOM during post_init.
- §5.10: NEW — `#[v8_async_method]` interaction.
- §5.11: NEW — telemetry / metrics counter.
- §5.13: NEW — documentation deliverable (plugin-system.md update).
- §6.1: added Tests #7 (with_state read), #8 (plain-Self ctor), #9 (callable_no_new + post_init), #10 (OOM); removed v1's `&Self`-dependent test wording.
- §6.2: concrete trybuild fixture filenames.
- §7.2: re-measured LoC with `wc -l`; v1 estimates were inflated 2-4×.
- §7.4: replaced "cherry-pick the hand-roll back" with a 7-day revert window.
- Open Q #1: cited workerd, Mozilla, N-API as references for the "post_init" name.
- Open Q #5: kept the (scope, this)-only decision; added quantitative cost argument for the Global+take overhead.
- Open Q #8: NEW — versioning policy.

v2 → v3 changelog
- §3.2: added single-thread/single-isolate scoping note.
- §3.2 → §5.3a: NEW section — cross-class private-symbol collisions; mandates class-prefixed naming convention; CI grep step.
- §4.4: REWROTE to address the "Self::Drop side effects delayed by GC" footgun. Documents lazy-drop as an explicit non-feature for v1; lays out the eager-drop alternative and why it's deferred.
- §4.5: FIXED the type-erroneous `with_state(...).flatten()` chain — `Option<(Option, Option)>` is not `Option<Option<T>>`. Replaced with explicit unpacking. Added the "do not call into JS inside the closure" comment.
- §4.5: clarified `pub(crate)` visibility for `after_install` so derived classes can chain.
- §4.6: renumbered diagram steps to match §1.4 prose exactly (4a/4b, 5/5b, 6).
- §5.4: added `pub(crate)` visibility note + scope-reborrow explanation; addressed the "base hook visibility" question explicitly.
- §5.5: documented the "do not call into JS inside `with_state` closure" convention (closures can capture `scope` from environment; the discipline is review-enforced).
- §5.7: replaced the hand-waved "doc attribute" claim with concrete `#[doc = concat!(...)]` content showing the expected hook signature.
- §5.10: added the unhandled-rejection sub-bullet (microtask rejection with no awaiter; matches existing async-method behavior).
- §5.13: added an enforced PR checklist; mentions PR-template reminder.
- §5.14: NEW — V8 startup-snapshot interaction (function-pointer stability invariant).
- §5.15: NEW — compile-time perf (negligible).
- §5.16: NEW — runtime perf budget + zerobench commitment.
- §5.17: NEW — CI lint commitments (un-prefixed priv-sym grep + future "hooks declared but unused" check).
- §6.1: split Test #5 into #5a (`&self` method call) + #5b (`&mut self` method call). Added Test #11 (stack-trace shape) + Test #12 (async work dispatch). Now 12 tests total.
- §7.1 step 5: clarified the per-class `with_*_state` naming convention (`with_state`, `with_byob_state`, `with_writer_state`, `with_ts_state`).
- §7.1 step 6: added the Request migration spike result — confirms `Headers::new` from inside `after_install` works.
- §7.2: replaced the "assume ~20% reduction" hand-wave with an explicit N+M+K decomposition; revised total savings to ~385 LoC (down from v2's 656).
- §7.4: replaced 7-day revert window with 3-day, justified by streams WPT nightly cadence.
- Open Q #4: quantified `Script::compile + run` cost (~50µs warm) to justify keeping `inherit_intrinsic` at install time.
- Open Q #8: added "consumers cargo-update freely without breakage" patch-bump note.
- Open Q #9: NEW — review-only changelog policy (strip before landing).

v3 → v5 (cumulative refinements)
- Header: V8 pin "v8 = 147" cited (resolves to v147.1.0 per Cargo.lock); MSRV declared unchanged.
- §5.13: PR template path verified at .github/PULL_REQUEST.md (note: not the GitHub-canonical PULL_REQUEST_TEMPLATE.md name).
- §5.16: clarified "happy-path budget includes call dispatch only, not user body work"; benchmark commitment names criterion-based crates/runtime/benches/v8_post_init.rs (distinct from zerobench).
- §5.3a: replaced grep-based lint sketch with full syn-walker test code (~50 lines); walkdir dependency status acknowledged honestly.
- §6.1 Test #11: split into #11 (stack shape) + #11b (paired hygiene fixtures with explicit "this proves the assertion logic itself works without false-positives" framing).
- §6.1 test count: reconciled to 13 (#1, #2, #3, #4, #5, #5b, #6, #7, #8, #9, #10, #11, #11b, #12). Header text updated.
- §4.5: condensed the verbose visibility doc-comment to one-line + §5.4 pointer.
- Open Q #5: added Request/Response caveat (with_state idiom not yet universal beyond streams).
- Process notes section: "v3 → v5 (cumulative refinements)" entry added; the moves are mechanical refinements, not design changes.
-->
