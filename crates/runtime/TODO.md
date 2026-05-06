# runtime TODO

Backlog ordered roughly by leverage.

## Done

### Crypto layout — promote kernel + lift node:crypto out of web/ (`feature/crypto-layout`)

The dual-surface crypto backend (`web/crypto/kernel/`) was promoted to
`base/crypto/` — it's not "web", it's the shared algorithm backend that
BOTH `web::crypto` (WebCrypto) and node:crypto call into. Lifting it to
`base/` (Chromium-style — foundational utilities shared across surface
APIs) makes the architecture visible and completes the three-way
symmetry with `web/crypto/` and `node/crypto/`.

`web/crypto_node/` was lifted to `node/crypto/`. node:crypto is a
Node-API surface, not a Web API. The new top-level `node/` folder
establishes the symmetry: `web/` for WHATWG/W3C, `node/` for Node-specific
APIs. Currently hosts only `crypto`, but is the natural home for
future native `node:*` modules (e.g. `node:zlib`, `node:os`).

Back-compat shims in `lib.rs` + `web/mod.rs` + `web/crypto/mod.rs` keep
all external paths resolving:
- `crate::crypto_node` (lib) → re-exports `web::crypto_node` → `node::crypto`
- `crate::web::crypto::kernel::*` → re-exports `crate::base::crypto`
- `crate::crypto_ops::*` → re-exports `crate::base::crypto` (for
  internal callers in `node::crypto`)

Pure rename + import-rewrite — zero semantic changes. Test counts match
baseline (16 crypto + 34 crypto_native + 69 crypto_node + 204 lib).

### Reorg the source tree (`feature/runtime-reorg`)

`crates/runtime/src/` is now grouped into four roots:

```
core/      runtime/state/dispatch/init/channel/modules/panic_util/plugin/cpu_timer/server/serve
webidl/    byte_string + clamp + enforce_range + usv_string
web/       Web API surface — base64, blob/, codec, crypto/, dom/, encoding/,
           fetch/{algorithms,body/,request,response,...}, headers, streams/,
           structured_clone, url/, websocket/
transport/ ssrf + client (per-thread cyper) + handler (kernel HTTP bridge)
```

Top-level survivors: `auth.rs`, `storage.rs`, `fetch_outcome.rs`, `embed/`
(now just `node-globals.js`). All old paths (`crate::fetch_native::…`,
`crate::dom::…`, `crate::state::…`, etc.) keep resolving via re-exports
in `lib.rs`, so external crates and tests didn't need migration.

Pure rename + import-rewrite — zero semantic changes. See commit message
for diff stats.

## Smaller items

- Per-class isolate-slot caching is per `__InstallSlot_X` types, but registration
  in `init.rs` happens via individual function calls. Could be unified via a
  `register_native_class!` macro — minor.
- `legacy_bridge.rs` (in `streams/`) is named misleadingly — still alive via
  `__zsBeginStreamForward`. Rename to `stream_bridge.rs` OR delete entirely if
  the Rust-only forwarder lands via fetch-js-delete.
- `state.rs` has `OpResult::StreamChunk` + `pending_fetches` etc. — some may be
  dead post-cleanup-rawfetch. Audit during reorg.

## V8 class macro migration follow-ups (consumer side)

The Tier 3 derives (`WebIdlDict`, `WebIdlEnum`, `v8_iterable`,
`#[v8_getter(same_object)]`) ship in `runtime-macros`. Existing
`web/` classes are migrated one-PR-per-class. Status:

**Done** (in this branch — `feature/v8-class-migrate`):

  - `Crypto.subtle` — `#[v8_getter(same_object)]` (`33a9fe7`)
  - Crypto enums: KeyType / KeyUsage / KeyFormat / NamedCurve —
    `#[derive(WebIdlEnum)]` (`e74d591`)
  - DOM event init dicts: EventInit / CustomEventInit /
    MessageEventInit / CloseEventInit — `#[derive(WebIdlDict)]`
    (`09b9ba8`)
  - Blob/File property bags: BlobPropertyBag / FilePropertyBag —
    `#[derive(WebIdlDict)]` (`fdd5681`)
  - QueuingStrategyInit — `#[derive(WebIdlDict)]` (`47948ad`)
  - WebSocketInit (zeroship-specific) — `#[derive(WebIdlDict)]`
    (`e226c8c`)
  - ReadableStreamIteratorOptions — `#[derive(WebIdlDict)]`
    (`5cbb8e0`)
  - TextDecoderOptions / TextDecodeOptions (shared by
    TextDecoder + TextDecoderStream) — `#[derive(WebIdlDict)]`
    (`9c16906`)
  - **Request** (whole-class) — `#[v8_class]
    #[v8_state_marker(Request)] impl RequestState`. MAC-01 Phase 2
    per `docs/proposals/macro-v8-state.md` §7.2. The unit struct
    `Request` continues to drive JS-class identity (install slot,
    brand check, callback names, `Symbol.toStringTag`); the impl
    block is on `RequestState` (the boxed state). Constructor +
    15 simple getters + 2 lazy-mint getters (headers / signal,
    plain `v8_getter` returning `Local<Object>` materialised from
    the state's `Global<Object>` — see migration commit body for
    why the macro's `same_object` flag is skipped for these two)
    + `clone()` are macro-emitted. `state_ptr` (private) and
    `build_kernel_request` (kernel fast-path) remain hand-rolled
    per design §7.2.1. PENDING_CT thread-local replaced with a
    local variable in the constructor body (the field-vs-local
    discussion in §7.2.4 settled: local is cleanest because the
    full state-build → headers-build → CT-apply flow stays
    monolithic in the macro constructor body).

**Deferred** (with reasons):

  - ~~`RequestInit` / `ResponseInit` / `RequestMode` /
    `RequestCache` / `RequestRedirect` / `RequestCredentials` /
    `RequestDestination` / `ReferrerPolicy` / `ResponseType`~~ —
    LANDED as #197. New module `web/fetch/enums.rs` defines the
    seven WebIDL enums; `RequestInit` / `ResponseInit` derive
    `WebIdlDict`; `RequestState` / `ResponseState` storage moved
    from `RefCell<String>` to `Cell<E>` for the typed-enum slots.
    Headers / body / signal stay as raw v8::Value passthroughs to
    preserve the explicit-null vs missing distinction the dict
    blanket can't express. Behaviour change: unknown enum values
    (`mode: "bogus"`) now throw TypeError per WebIDL §3.13.7
    instead of silently storing the string.
  - `RedirectMode` / `CredentialsMode` (`web/fetch/algorithms.rs`)
    — still hand-rolled because they live on the algorithm-side
    `FetchRequest` (not on the JS-facing `RequestState`). The JS
    boundary uses `RequestRedirect` / `RequestCredentials` (typed,
    spec-validating); a `From` bridge in `enums.rs` converts to
    the algorithm shape after `snapshot_request` reads the typed
    state. Migrating these to `WebIdlEnum` is unnecessary because
    they never face JS — they're purely an internal kernel form.
  - `ReadableStreamGetReaderOptions` / `ReadableStreamReaderMode`
    — hand-rolled `getReader(options)` parser uses `v8::tc_scope!`
    to capture + rethrow user-thrown errors from a custom
    `mode.toString()`, preserving the exact exception value. The
    macro's `WebIdlEnum::from_v8` would replace it with a generic
    TypeError.
  - `ReadableStreamType` — `is_byte_stream` parsing is one branch
    (`type === "bytes"` → bool); not enough surface for a derive.
  - WebSocket `BinaryType` enum — silent no-op on unknown values
    per WPT `binaryType-wrong-value.any.js`. The macro's
    `WebIdlConvertible` throws TypeError on unknown — different
    behaviour.
  - WebCrypto algorithm-init dicts — many variant-tagged shapes,
    custom validation; not a clean derive fit yet.
  - Crypto `HashAlgo` enum — spec mandates case-insensitive matching
    ("SHA-256" / "sha-256" / "Sha-256" all valid); the macro's
    auto-`from_str` is case-sensitive.
  - `AddEventListenerOptions` / `EventListenerOptions` —
    `(EventListenerOptions or boolean)` union (boolean shorthand
    for `capture`), AND `signal: null` is a TypeError (not the
    dict's default-construct path). Neither is expressible by
    the dict derive today.

**Iterator migrations** — MIGRATED via MAC-09 (+ MAC-14 for FormData):
  - `HeadersIterator` → `#[v8_iterable(mode = live)]`. `value_pairs(&mut self)`
    wraps the lazy `sorted_cache` rebuild. `5677051` (-333 LOC).
  - `URLSearchParamsIterator` → `#[v8_iterable(mode = live)]`.
    `value_pairs(&mut self, scope)` calls `sync_from_parent(scope)` then
    returns the entries. `da77c03` (-314 LOC).
  - `FormDataIterator` → `#[v8_iterable(mode = live, value_marshal
    = entry_value_to_v8)]`. The `(USVString or File)` union goes through
    the user-supplied marshal hook so File entries preserve V8 object
    identity. `cc945cc` (-214 LOC).
  Net consumer savings: ~-861 LOC across the three migrations.

**MAC-02 streams constructor migration** (`#[v8_constructor(post_init = "...")]`):

The four hand-rolled streams classes have constructors that allocate a
PromiseResolver / state Box / private symbol AFTER the V8 wrapper exists.
MAC-02 (the `post_init` hook) shipped to unblock these. Status:

  - `ReadableStreamDefaultReader` — `#[v8_class]` + post_init,
    `acquire_*` keeps the manual Rust-side path. Methods (read /
    releaseLock / cancel / closed) stay raw FunctionCallbacks (they
    need direct `args.this()` access for priv-sym reads + Promise
    allocation, and converting them to `#[v8_method]` is a separate,
    much larger refactor). Migrated in this branch.
  - `ReadableStreamBYOBReader` — `#[v8_class]` + post_init, parallel
    to DefaultReader. The `BYOB_READER_TAG_SLOT` priv-sym + the
    ReaderGenericInitialize logic share a `finalize_byob_reader`
    helper between the JS path's `after_install` hook and the
    Rust-side `set_up_byob_reader_internal` path. Migrated in this
    branch.
  - `WritableStreamDefaultWriter` — `#[v8_class]` + post_init. The
    constructor body validates the stream + lock, post_init writes
    the WRITER_BRAND priv-sym (must come before any `with_state`
    call, since `with_state` brand-checks) and runs the four-way
    `WSState` dispatch that initializes closedPromise / readyPromise.
    The state-dispatch logic lives in `finalize_writer`, shared
    between `after_install` and `setup_writer_internal` (used by
    `acquire_writable_stream_default_writer`). Migrated in this
    branch.

**Deferred from MAC-02 phase 2** (with reasons):
  - `TransformStream` — bailed during phase 2. The constructor body has
    THREE distinct "fail with V8 pending exception" paths that the
    original code handles with direct `return;` after a peer helper
    threw on the `scope`:

      1. `parse_strategy_local(scope, writable_strategy, 1.0)` returns
         `Result<_, ()>` with V8 already holding the pending exception.
      2. Same for `parse_strategy_local(scope, readable_strategy, 0.0)`.
      3. `set_up_transform_stream_default_controller_from_transformer`
         returns `Err("TransformStream: start threw synchronously")`
         WHEN the user's `transformer.start()` threw — the exception is
         already pending in V8 and the original code's
         `if msg != "TransformStream: start threw synchronously"` branch
         deliberately suppresses pushing a second TypeError.

    Translating these paths through `Self::new() -> Result<Self, OpError>`
    + macro-emitted error-mapping requires either (a) capturing each
    pending exception via `tc_scope!` and converting to
    `OpError::js_value(scope, exception, msg)` (the JsValue passthrough
    variant) at every call site, or (b) refactoring
    `parse_strategy_local` and the controller-from-transformer helper
    to return `Result<_, OpError>` directly. Both are larger than the
    constructor migrations of the readers / writer (which had clean
    `Result<(), String>` setup helpers with no JS-thrown side-effects).

    Combined with the 6 pre-existing `wpt_streams_transform` failures
    (DEFERRED — slot-shared finishPromise refactor), the risk-reward of
    migrating the constructor without also refactoring the helpers it
    depends on is poor.

    Recommendation: land the slot-shared finishPromise refactor first
    (which already touches
    `set_up_transform_stream_default_controller_from_transformer`),
    then port the TransformStream constructor with the helpers updated
    in lockstep.

  Per-class method migration (`#[v8_method]` for read / releaseLock /
  cancel / write / etc.) is a follow-up after the constructor cluster
  lands. The bulk of the design's LoC savings live in method migration,
  not the constructor.

**MAC-01 fetch class migration** (`#[v8_state_marker(MarkerTy)] impl StateTy`):

Per design `docs/proposals/macro-v8-state.md` §7, the hand-rolled fetch
classes migrate onto the macro under state-projection — the unit `Request`
/ `Response` markers drive JS-class identity (install slot, brand check,
callback names) while the boxed `RequestState` / `ResponseState`
structures carry the per-instance state. Status:

  - `Response` — `#[v8_class] #[v8_state_marker(Response)] impl
    ResponseState`. Constructor + 8 getters + `clone()` migrate.
    `install_global` shrinks to a hand-rolled wrapper around
    `Response::install` that adds (a) the body consumer methods via
    `install_body_methods::<Response>`, (b) the
    `ResponseTemplateSlot` cache used by `build_kernel_response`, and
    (c) the three static methods (`error` / `redirect` / `json`).
    Static methods stayed hand-rolled in this PR — see the gap note
    on the impl block: the macro's `gen_static_callback` emits
    `<MarkerTy>::method(...)` for the call expression, which under
    `#[v8_state_marker]` resolves against the unit marker rather
    than the impl receiver. A 3-line macro fix (mirror
    `gen_method_callback`'s use of `state_ty`) lifts them into the
    macro impl block; tracked as a follow-up. `headers::seal_immutable`
    on `Response.error()` survived the migration verbatim (preserved
    inside the hand-rolled `static_error_callback`). LOC delta:
    1137 → 1065 (-72; floor set by the spec walks + the static
    method bodies retained per the macro gap above).

  - `Request` — Phase 2, in progress on a separate worktree.

**Macro follow-ups** (small, well-scoped fixes):

  - `gen_static_callback` should dispatch through `state_ty` instead
    of `class_ty` when `#[v8_state_marker]` is present. Mirrors the
    existing `gen_method_callback` shape. Unblocks Response's three
    static methods migrating onto `#[v8_static_method]` purely
    additively.

## Memory footprint

The runtime's per-isolate working set sits around 125 MB after warmup
(V8 baseline ~50 MB + native class init ~20 MB + scenarios bytecode
~20 MB + transient request state ~30 MB). At 16 workers per process,
that's ~2 GB resident. Three levers, listed by effort × impact:

### 1. Per-isolate `--max-old-space-size` cap — SHIPPED 2026-05-06

Wired via `RuntimeBuilder::heap_limit_mb(mb)` →
`Isolate::CreateParams::heap_limits(0, max)`. Default 128 MB; control
plane surfaces `AppRuntimeLimits.heap_limit_mb` per app. The
near-heap-limit callback grows the cap by `initial / 4` per hit
(escape valve, capped at 4 × initial) and calls
`IsolateHandle::terminate_execution` once hits ≥ 5 — surfaces as a
catchable JS RangeError, then the worker LRU reaps the isolate.

Reference docs: `docs/reference/runtime-limits.md`.
Tests: `crates/runtime/tests/heap_limits.rs` (3 tests).

### 2. Boot snapshot — INVESTIGATED 2026-05-06, NOT WORTH IT

We tried this through three implementation phases (`#190` Phase 1 +
Phase 2 + Phase 2b on the now-deleted `feature/boot-snapshot`
branch). All three were perf-neutral or regressed `isolate_only` p50
by 3-5%. Root cause: the TODO's "50–200 ms → 5–10 ms" target was
based on Cloudflare Workers' baseline (heavy polyfills, dozens of
native classes, larger Workers runtime). Our actual cold-start is
**~2.4 ms** for `boot_to_first` — there's no 45 ms to save.

Where our cold-start time goes (rough decomposition):

  - V8 isolate creation: ~1 ms (V8 intrinsic, can't snapshot away)
  - JS evaluation (polyfills + bootstrap): ~0.5–1 ms
  - Native class installs: ~50–100 µs total
  - Module loading + instantiation: rest

The snapshot would only capture native class installs (smallest
slice). To move the needle we'd have to capture the JS-evaluation
heap — a different architectural lever (run polyfill JS at build
time, freeze the post-eval heap). That's a separate proposal, not
the simple "freeze native classes" snapshot the TODO described.

V8 SnapshotCreator also has tight constraints (every callback in
`external_references`, every cached `v8::Global` dropped before
`create_blob`, no `Weak::with_guaranteed_finalizer`). Headers'
fastcall and Crypto's per-realm finalizer hit panic walls during
the experiment — fixing them would require runtime-macros patches
and per-class debug, with no proportional win.

**Verdict:** the platform's discipline of small native primitives +
lazy lib imports + minimal polyfill JS already won most of the
cold-start battle. Revisit only if cold-start exceeds ~10 ms (e.g.,
a heavier polyfill / native surface ships).

If revisited, prefer the lazy-in-process snapshot pattern over a
build.rs blob: V8 won't accept flag changes after `init_v8`, so
`SnapshotCreator` at build time has ergonomic issues. Build the
snapshot at first `Runtime::builder().build()`, cache in a
process-wide `OnceLock<Vec<u8>>`, share across all subsequent
isolates. The discarded `feature/boot-snapshot` branch in git
history (last commit `2bcaef2`, dropped 2026-05-06) carries the
working scaffolding for this pattern.

### 4. Idle GC trigger — SHIPPED 2026-05-06

Wired via `RuntimeBuilder::idle_gc_after_ms(ms)` (default 30 s; pass
0 to disable). Per-isolate compio ticker holds `Weak<RuntimeInner>`,
fires `Isolate::low_memory_notification` on quiet windows. The
v8 = "147" Rust binding doesn't expose `idle_notification_deadline`;
`low_memory_notification` is the closest substitute (synchronous
full GC instead of incremental budget — invisible since it only
fires on idle).

Reference docs: `docs/reference/runtime-limits.md` § Idle GC.
Tests: `crates/runtime/tests/idle_gc.rs` (3 tests).

(Note: original "drop --workers=16 to --workers=4" alternative
isn't a runtime concern — it's a deploy-time config.)

## Test infrastructure

- Hand tests + WPT runners are scattered across `tests/` flat. After reorg, mirror
  the new src layout: `tests/web/fetch/...`, `tests/web/url/...` etc.
- WPT runners share a lot of testharness shim code (sanitize, fetch-fixture stub,
  microtask draining loop). Extract to a shared `tests/wpt_harness.rs` module.
- The microtask drain loop (`for _ in 0..10 { scope.perform_microtask_checkpoint() }`)
  is hard-coded; flaky under chained-await patterns. Loop until stable.
