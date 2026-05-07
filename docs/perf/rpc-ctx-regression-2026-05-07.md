# RPC ctx regression — investigation 2026-05-07

**TL;DR.** Commit `9be897d` (RPC v2 phase 1 — ALS-backed ctx + frozen Headers/URL)
introduced an unconditional, eager per-request `RpcContext::build_js_object`
on the Tier-1 RPC fast path. The path now performs ~10–15 V8 object
allocations, ~6–10 Rust heap allocations, two frozen-class shadow installs,
two `Object.freeze` reentries, and an ALS-map clone+install+restore on every
`POST /_zs/v1/<id>` — even when the user procedure (e.g. `ping = () => "pong"`)
never reads ctx. The microbench (`rpc_dispatch.rs`) failed to detect the
regression because it runs single-threaded against a synchronous echo, never
sets `app_id` (so the AbortGuard registration is skipped), and is dominated
by JSON.stringify of the echoed body. Under the 16-worker macrobench, the
fixed-cost-per-request bloat plus per-allocation cache thrash plus GC
pressure across 16 isolates collapses ping from **1,162,544 req/s
(p50 250 µs)** to **220,970 req/s (p50 1.2 ms)** — an 81 % throughput drop.

The fix space is large and the recommended sequencing is **(1) lazy ctx via
materialize-on-first-`__zeroshipGetRpcCtx()` (≥70 % recovery, ~150 LOC)**,
optionally followed by **(2) reset-in-place pooled ctx** for the eager case,
**(3) drop the redundant `Object.freeze` calls** (Headers' `Immutable` guard
already throws), and **(4) Vite-plugin static analysis** to strip ALS install
on procedures that are statically proven not to read ctx.

---

## Section 1 — Current state of the hot path

Source of truth:

- `crates/runtime/src/core/runtime.rs:1389-1589` — `call_fetch_handler` (Tier 1 RPC dispatch)
- `crates/runtime/src/core/runtime.rs:3146-3178` — `build_rpc_context_from_request`
- `crates/runtime/src/core/runtime.rs:3189-3212` — `call_rpc_inner`
- `crates/runtime/src/rpc/dispatch.rs:69-160` — `RpcContext::build_js_object`
- `crates/runtime/src/rpc/dispatch.rs:236-313` — `seal_url`, `install_throwing_setter`
- `crates/runtime/src/rpc/dispatch.rs:368-395` — `with_rpc_context_in_als`
- `crates/runtime/src/web/headers.rs:798-913` — `build_kernel_headers`, `seal_immutable`
- `crates/runtime/src/web/dom/abort_signal.rs:240-278` — `mint_abort_signal`
- `crates/runtime/src/web/dom/abort_controller.rs:42-72` — AbortController constructor
- `crates/runtime/src/node/async_hooks/als.rs:277-323` — `read_context_map`, `clone_map`

### Step-by-step ping path (HEAD = `d2e7e22`)

| # | What runs | Where | Order-of-magnitude cost |
| - | --------- | ----- | ----------------------- |
| 1 | Reset idle-GC clock | `runtime.rs:1400` | ~10 ns (atomic store) |
| 2 | `state.borrow_mut().set_env_snapshot(env)` | `runtime.rs:1417` | ~50 ns (RefCell + memcpy) |
| 3 | `ensure_initialized` (no-op after first call) | `runtime.rs:1419` | ~20 ns (branch) |
| 4 | Allocate `request_id` (`u64++`) | `runtime.rs:1450-1451` | ~5 ns |
| 5 | `state.borrow_mut()` + write `executing_request_id` + `executing_request_cancel` | `runtime.rs:1457-1461` | ~100 ns (RefCell + Arc clone) |
| 6 | WS-upgrade scan over `headers` slice | `runtime.rs:1481-1483` | ~50 ns (≤4 headers in bench) |
| 7 | `extract_zs_v1_id` | `runtime.rs:1485` | ~30 ns (pure-Rust slice) |
| 8 | `arm_cpu_timer()` | `runtime.rs:1499` | ~50 ns |
| 9 | `enter_v8!` → V8 isolate enter + HandleScope + ContextScope | `runtime.rs:1512` | ~100–200 ns (lock acquire, isolate slot setup) |
| 10 | `v8::Local::new` for `rpc_fn` | `runtime.rs:1519` | ~10 ns |
| 11 | `v8::String::new` for `rpc_id` arg | `runtime.rs:1520` | ~50 ns + V8-internal string alloc |
| 12 | `parse_rpc_input` (V8 JSON.parse on body) | `runtime.rs:1521-1526` | ~500 ns – 5 µs (size-dependent; tiny ≈ 1 µs) |
| 13 | Read `state.ctx_obj` singleton + `v8::Local::new` | `runtime.rs:1528-1532` | ~30 ns |
| 14 | **Look up `per_request_user.get(&request_id).cloned()`** | `runtime.rs:1550` | ~50 ns — none for ping (anon) |
| 15 | **`build_rpc_context_from_request`** — clones headers Vec, `to_string()` for method/url, two `format!("req_{:016x}", …)` allocations, scans headers for Idempotency-Key + traceparent | `runtime.rs:1551-1553`, `runtime.rs:3146-3178` | ~400 ns – 1 µs (3 String allocs + 1 Vec<(String,String)> clone — 2 fresh String per header pair = ~6 small heap allocs for the bench's 1–2 header) |
| 16 | **`RpcContext::build_js_object`** — see §2 below for full breakdown | `dispatch.rs:79-159` | **~2.5–4 µs** dominant fixed cost |
| 17 | **`register_in_flight`** when `app_id.is_some()` — global thread-local `RefCell<HashMap>` insert keyed by `(Uuid, u64)`, value is `v8::Global<v8::Object>` (controller). Returns RAII guard. | `runtime.rs:1556-1563`, `rpc/abort.rs:88-115` | ~100–200 ns (HashMap insert + Global::new) |
| 18 | **`with_rpc_context_in_als`** — `get_continuation_preserved_embedder_data`, allocate `v8::Global` snapshot, mint platform Symbol on first request (cached after), read existing map, **clone every entry** of that map into a fresh `v8::Map`, set the rpc-ctx entry, `set_continuation_preserved_embedder_data` | `dispatch.rs:368-395`, `als.rs:300-323` | **~300 ns – 1 µs** — empty map case (bench) is closer to 300 ns; with user ALS entries grows linearly |
| 19 | `tc_scope!` + `rpc_fn.call(undefined, [id, input, ctx])` | `runtime.rs:3199-3208` | ~200–500 ns (V8 function call + 3-arg adapter) — **the user procedure itself** |
| 20 | Restore embedder-data slot | `dispatch.rs:391-392` | ~50 ns |
| 21 | `perform_microtask_checkpoint` | `runtime.rs:3214` | ~100 ns when queue empty |
| 22 | `classify_rpc_return` + JSON.stringify of `"pong"` | `runtime.rs:~3230+` | ~500 ns |
| 23 | `register_in_flight` guard `Drop` (HashMap remove) | `rpc/abort.rs:88-95` | ~100 ns |
| 24 | Build response Vec + headers + body String | runtime tail | ~200 ns |
| 25 | `enter_v8!` exit + state cleanup | runtime.rs cleanup | ~100 ns |

**Total estimated dispatch fixed cost (post-9be897d):** ~5–8 µs/request from
steps 14–18 + 23 alone. The pre-regression path skipped 14–18 + 23 entirely
(it called `call_rpc_inner` with `als_ctx_object: None`).

---

## Section 2 — Cost breakdown (4 µs/request budget)

The macrobench delta is **p50 250 µs → 1200 µs at 16w**, but per-thread that's
about **4 µs of new fixed cost per call** scaled up by isolate-count
contention. Decomposition (estimates; one-shot bench microsamples not
re-run for this report — flagged as Open Question §6):

| Sub-cost | Source | Per-call estimate | Notes |
| -------- | ------ | ----------------: | ----- |
| **Headers Vec clone** | `runtime.rs:3175` `headers.to_vec()` | ~200 ns | Each `(String, String)` is 2 heap copies; bench has 1–2 headers, prod ~10. |
| **Native Headers wrapper construction** | `headers.rs:798-828` (`new_instance` + Box + `set_internal_field` + per-pair `list_append_unchecked`) | ~600 ns–1 µs | 1 V8 instance alloc, 1 Box alloc, n per-pair Vec<u8> allocs, finalizer registration. |
| **`seal_immutable` + `Object.freeze` on Headers** | `dispatch.rs:131-132` | ~400 ns | `seal_immutable` is a single guard write (~10 ns); `Object.freeze` is a JS-call out via `Function.call` — V8 must walk the object's own properties, transition its hidden class to "frozen", and commit the new map. |
| **Native URL construction via `new URL(href)`** | `dispatch.rs:167-184` | ~800 ns–2 µs | Goes through the user-visible URL constructor → ada-url parse + `URLSearchParams` companion alloc + GC-finalizer wiring. The URL parser is known to be ~500 ns–1 µs even for simple `http://localhost/_zs/v1/ping`. |
| **`seal_url` shadow installs** | `dispatch.rs:236-283` | ~600 ns–1 µs | 10 setter shadows (`href`, `protocol`, `username`, `password`, `host`, `hostname`, `port`, `pathname`, `search`, `hash`) — each is a `get` + `define_property` with a `PropertyDescriptor`. Plus 4 mutator shadows on `searchParams` (`set`, `append`, `delete`, `sort`). Plus the throwing-fn allocation. **14 `define_property` calls per request**, each forces a hidden-class transition. |
| **`Object.freeze` on URL + URLSearchParams** | `dispatch.rs:145, 281` | ~300 ns | Two more freeze callouts. |
| **AbortController construction** | `dispatch.rs:190-210`, `abort_controller.rs:42-72`, `abort_signal.rs:240-278` | ~500 ns–1 µs | Each controller calls `mint_abort_signal` which: `inst_tmpl.new_instance` (V8 alloc), `Box::new(AbortSignal::default())`, `External::new`, `set_internal_field`, prototype set, `attach_listeners` (Rc creation), `Weak::with_guaranteed_finalizer` (registers a finalizer slot). Then the controller wrapper itself does the same. Plus a second `Function::call`-style `signal` getter call to extract the v8::Object. |
| **ALS map install/restore** | `dispatch.rs:368-395`, `als.rs:300-323` | ~300 ns–1 µs | `clone_map` allocates a fresh `v8::Map`, calls `as_array` (V8 emits a flat `[k0,v0,k1,v1,...]`), then n `arr.get_index` + `dst.set` per existing key. Empty case (bench, no user ALS) ≈ 300 ns; the embedder-data slot snapshot/restore adds 2 `Global::new` → 2 weak handles. |
| **`format!("req_{:016x}", id)` × 2** + `method.to_string()` + `url.to_string()` | `runtime.rs:3170-3174` | ~150 ns | 4 small heap allocs. |
| **AbortGuard HashMap insert + drop** | `rpc/abort.rs` | ~150 ns | Thread-local `RefCell` borrow + HashMap insert. `Global::new` for the controller. Drop on sync-return path. |
| **V8 string allocations for property names** | `dispatch.rs:93-152` | ~300 ns | `requestId`, `traceId`, `method`, `idempotencyKey`, `user`, `headers`, `url`, `signal` — 8 V8 String allocs per request. Cacheable as `OneByteConst`. |
| **Per-isolate slot mint on first call** | `dispatch.rs:347-356` | amortized ~0 | One-time. |

**Total: ~4–7 µs of new fixed cost per request.** The two largest
contributions are **(a) URL construction + sealing (~2 µs combined)** and
**(b) Headers + AbortController construction (~1.5 µs)** — both of which
the user procedure for `ping` literally never reads.

Cross-check against `crates/runtime/benches/results-2026-05-05-rpc-v2-dispatch.txt:91-99`
which estimated `RpcContext::build_js_object ~10-15 µs`. That estimate is
stale: `seal_url` was recently expanded to install per-setter shadows
(see `dispatch.rs:254-260`) — but the headline 10–15 µs is in the right
ballpark for combined ctx construction across all flavors.

The macrobench is sensitive beyond per-call latency:

- 16 isolates × ~10 heap allocs per request × 200K req/s = **32M
  allocations/sec across the worker** → allocator lock contention,
  GC churn (V8 minor GC frequency rises sharply once the Eden pool
  fills, every minor GC stalls the isolate ~50–200 µs).
- The shadow `define_property` calls each invalidate inline-cache slots
  on the URL/URLSearchParams classes' hidden-class chain. The
  per-call freshness means V8 cannot stabilize the inline cache — every
  request sees IC-cold setters.
- `format!` + `String::clone` + `Vec::clone` on the headers slice, even
  small, will overflow the per-thread allocator's small-bin caches at
  this rate.

The microbench misses all of this because it (a) runs one isolate
(no allocator contention), (b) is single-threaded (no GC stop-the-world
fan-out), (c) builds with `Runtime::builder()` without `app_id` so the
AbortGuard path is skipped (`runtime.rs:1556-1563`), and (d) the echoed
body's JSON.stringify dominates the sample (large = 350 µs vs tiny = 30 µs).

---

## Section 3 — Semantic constraints

What the proposal commits to (`docs/proposals/rpc-v2.md` §3, lines 250–262):

- **Always-present fields:** `user`, `requestId`, `traceId`, `signal`,
  `idempotencyKey`, `headers`, `method`, `url`, `waitUntil`, `log`, `env`,
  `meter`. Currently implemented: the first 8 (no `waitUntil`/`log`/`env`/`meter`
  on `als_ctx_object` — those still live on the singleton third-arg `ctx`).
- **Stable identity (must be the same JS object across reads):**
  `ctx.signal` is the most load-bearing — `setTimeout` / `fetch(_, {signal})`
  pin the same signal, and `addEventListener("abort", …)` listeners on it
  must fire when eviction triggers `controller.abort()`. Today this is
  enforced because `dispatch.rs:154-158` builds the controller once per
  request and the `signal` getter goes through `[SameObject]`
  (`abort_controller.rs:54-57`).
- **Frozen surface (observable):**
  `ctx_ctx.rs::frozen_headers_set_throws` (line 54) and
  `frozen_url_searchparams_set_throws` (line 97) test that mutators throw
  `TypeError`. **However:** the Headers `Immutable` guard alone already
  throws (`headers.rs:163-167`) — `Object.freeze` is defense-in-depth.
  URL setters and `searchParams.set` rely on the shadow install + freeze
  combination, but spec-correct alternatives exist (per §4).
- **`Object.isFrozen(ctx.headers)` / `Object.isFrozen(ctx.url)`:** not
  asserted in tests today (search of `rpc_ctx.rs` finds no `isFrozen`),
  but the freeze is part of the proposal's wording (§3 lines 256, 258
  — "kernel constructs the Headers instance and calls Object.freeze() on
  the JS wrapper"). User code may legitimately introspect with `isFrozen`.
- **Ambient survival across awaits:** `ctx_survives_await` (`rpc_ctx.rs:149`)
  and `ctx_survives_promise_then` (line 170) require ALS-backed
  propagation. This is the load-bearing feature 9be897d shipped.
- **No collision with user `AsyncLocalStorage`:** `user_async_local_storage_does_not_collide`
  (`rpc_ctx.rs:270`) — the platform Symbol (`zs:RpcContext`) is distinct
  from any user-minted ALS Symbol.
- **Auth model:** `ctx.user` reads `state.per_request_user[request_id]`
  (`runtime.rs:1550`) — this is the gateway-injected `ZeroShip-User`
  payload (set via `worker::set_request_user` before dispatch). The proposal's
  `ctx.user` is required to be present *before* user code runs for
  `auth: "user"`/`"admin"` procedures, so auth-gate evaluation cannot be
  deferred past the first procedure-side read. **However** — at the
  worker level, if `auth: "anon"`, the user is `null` and no
  pre-population is required.
- **`Object.freeze` on the ctx top-level object itself:** not currently
  applied (`build_js_object` returns `obj` unfrozen — only `headers` and
  `url` are frozen). The proposal §3 doesn't require it; only the
  contained Headers/URL.

**Implication for solutions:** `ctx.signal` identity must be stable
within a request; lazy creation must be deterministic per request, not
per access. Frozen Headers/URL behavior is observable but the
cheaper-throw path (the existing `Immutable` guard for Headers) is
spec-equivalent.

---

## Section 4 — Solution enumeration

Each candidate is independent unless noted. Speedup % is "fraction of
the 942K req/s gap (1162K → 220K) recovered" estimated from the §2
breakdown.

### S1 — Lazy ctx via `__zeroshipGetRpcCtx()` materialize-on-first-read
**Mechanism.** Replace the eager `RpcContext::build_js_object` call site
with stashing a lightweight Rust `RpcContext` (no V8 work) in a per-isolate
slot, plus an empty placeholder in the ALS map. `__zeroshipGetRpcCtx()`
becomes the materializer: on first call within a request it builds the
JS object, replaces the placeholder, returns it. Subsequent calls return
the cached Local. Procedures that never call `__zeroshipGetRpcCtx()` (the
ping bench fixture, plus most simple user procedures during the early
adoption window) pay zero V8-allocation cost.
- **Speedup:** **~70–85 % of the gap** when ctx is unused (the bench case)
  — eliminates steps 14–18 entirely. Even when ctx IS used, only one
  field-getter is read on average, so a fully-lazy-per-field design
  could shave another 30 % off the read case.
- **Effort:** ~150 LOC. Touches `dispatch.rs`, `runtime.rs:1550-1567`.
  Complexity 2/5.
- **Risk:** Must preserve `ctx.signal` identity. Solution: register the
  AbortController with the registry eagerly (it's only ~150 ns) so eviction
  still fires; but defer minting the *V8 wrapper* until first read. **One
  semantic change:** `ctx.signal` becomes a property the materializer mints
  on demand — but JS code that holds a reference to the same `ctx` object
  still sees the same signal because materialization is one-shot per
  request.
- **Compatibility:** No user-facing API change. Vite-plugin neutral.
- **Stacking:** Composes with S3 (drop redundant freeze), S5 (pooled
  classes), S6 (ObjectTemplate accessors). Subsumes parts of S2.

### S2 — Vite-plugin static analysis: skip ALS install for ctx-free procedures
**Mechanism.** The vite-plugin's transform pass already walks the procedure
import graph for "use server" detection (`docs/proposals/rpc-v2.md` §1).
Extend it to detect whether any imported binding from `@zeroship/server`
is actually used by the procedure or its transitive imports. Annotate
the synthetic entry's `_procedures[wid]` with `{ needsCtx: false }`. The
kernel reads this at module-init and falls back to the pre-9be897d
`call_rpc_inner(…, None)` path for those procedures.
- **Speedup:** **~80 % of the gap** for procedures statically proven
  ctx-free. 0 % for procedures that import `ctx`.
- **Effort:** ~300 LOC across vite-plugin + kernel. Touches transform,
  metadata wire shape, dispatch lookup. Complexity 4/5.
- **Risk:** Static analysis is conservative — must mark "uncertain"
  cases as `needsCtx: true` (any indirect call that the analyzer can't
  prove ctx-free). False-negatives = correctness violation
  (`__zeroshipGetRpcCtx()` returns undefined inside a procedure that
  expects it). Tests: existing `rpc_ctx.rs` suite must still pass for
  procedures that DO import ctx.
- **Compatibility:** Requires vite-plugin coordination. The analysis must
  also handle dynamic `import()` and ESM re-exports.
- **Stacking:** Redundant with S1 — pick one. S1 is cheaper to implement
  but pays a ~50 ns per-procedure-call overhead (the `__zeroshipGetRpcCtx`
  miss path); S2 has zero runtime overhead but build-time complexity.

### S3 — Drop the redundant `Object.freeze` on Headers + drop seal_url's per-setter shadows
**Mechanism.** The Headers `Immutable` guard (`headers.rs:163-167`) already
throws `TypeError` on `set`/`append`/`delete`. `Object.freeze` on the
wrapper adds nothing user-observable for those methods (the spec already
mandates the throw via the guard). Similarly for URL: the spec's URL
constructor returns a non-frozen object; `seal_url` is a platform-imposed
freeze for ctx that costs ~14 `define_property` calls. Replace with: a
single `is_immutable_url` boolean on the wrapper's internal slot, checked
by the `#[v8_setter]` callbacks for href/host/etc. This removes the
shadow installs entirely.
- **Speedup:** **~15–25 % of the gap.** Removes ~600 ns – 1 µs of seal_url
  cost + ~300 ns of `Object.freeze` callouts.
- **Effort:** ~80 LOC. Touches `dispatch.rs:131-145` and adds an
  `Immutable` guard slot to URL similar to Headers'. Complexity 2/5.
- **Risk:** Spec-equivalent; the throw still happens via the guard.
  `Object.isFrozen(ctx.headers)` becomes false, which **is** observable.
  Mitigations: keep `Object.freeze` on the wrapper (cheap, single call);
  drop only the per-setter shadow installs.
- **Compatibility:** No API change. Internal refactor only.
- **Stacking:** Composes cleanly with S1.

### S4 — Pass headers as a Map-shaped JS object; lazy upgrade to Headers class on `.headers` access
**Mechanism.** Replace `ctx.headers = <native Headers instance>` with
`ctx.headers = <plain object {accept: …, content-type: …}>` for the common
case. If user code calls `ctx.headers.get('foo')` (Headers method), trap
the access via a Proxy or a getter on `ctx.headers` that materializes a
real Headers instance on demand. Or simpler: ship a single `ctx.headersRaw`
plain map plus `ctx.headers` as a *getter* on `ctx` that materializes.
- **Speedup:** **~15–20 % of the gap.** Eliminates `build_kernel_headers`
  + freeze.
- **Effort:** ~100 LOC, plus a docs change to clarify that `ctx.headers`
  is lazily built. Complexity 3/5.
- **Risk:** Changes the *type* observable to user code (Map-shape vs
  Headers class). The proposal §3 commits to "Headers (native, frozen)"
  — this is a semantic change requiring a proposal amendment.
- **Compatibility:** Subtle user-visible change. `Object.keys(ctx.headers)`
  works on the plain object but not on Headers (which iterates via
  `entries()`).
- **Stacking:** Subsumed by S1. Don't ship both.

### S5 — Pooled / reused Headers/URL/AbortController templates with `reset()`
**Mechanism.** Per-isolate slot pool of pre-built native wrappers. On
each request, mutate (in Rust) the underlying Box state to point at the
new headers/href, then hand out the same JS Local. Requires:
(a) bypassing the `Immutable` guard during reset (a private setter); (b)
making the wrapper's hidden class stable enough that V8 doesn't deopt;
(c) per-request `Object.freeze` is incompatible with mutation, so freeze
must move to a "logical immutable" check.
- **Speedup:** **~50–60 % of the gap.** Eliminates wrapper allocs but
  keeps the property-name string allocs and the `obj.set` calls.
- **Effort:** ~400 LOC across headers + URL + abort_signal. Complexity 5/5.
- **Risk:** Hidden-class stability. V8 IC will sometimes still deopt.
  AbortController reset is *especially* fraught — listeners attached
  during request N could fire during request N+1 if the controller isn't
  fully drained. The proposal contract (`ctx.signal` aborts on isolate
  eviction) means the controller must be a fresh observable identity per
  request. Pool-with-reset would require a separate `signal` Global per
  request even if the controller wrapper is shared.
- **Compatibility:** Internal only.
- **Stacking:** Composes with S1 but partially redundant — if ctx is
  lazy, the per-request wrapper alloc only fires when ctx is read.
  Revisit S5 only if S1's "ctx is read" case still dominates.

### S6 — ObjectTemplate with internal-field-driven accessors, populated lazily
**Mechanism.** Replace `v8::Object::new(scope) + n × obj.set(…)` with a
single `ObjectTemplate::new_instance` whose accessors read from a Box
hanging off internal field 0. The accessors compile to property lookup
+ external dereference. Construction cost drops from "n V8 String allocs +
n property writes" to "1 instance alloc + 1 Box alloc + 1 internal field
write". Field reads are accessor-driven so the Headers/URL materialization
inside an accessor is free for the ping case.
- **Speedup:** **~40–60 % of the gap.** Subsumes much of S1.
- **Effort:** ~250 LOC + an `RpcCtx` `#[v8_class]` plus accessor methods.
  Touches `runtime-macros` if any. Complexity 4/5.
- **Risk:** The proposal exposes `ctx` as a real frozen object — accessor
  approach makes it a class instance with a custom `Symbol.toStringTag`.
  Tests against `Object.keys(ctx)` would break (own-properties vs prototype
  accessors). Solvable with an own-property accessor descriptor at
  `ObjectTemplate::set_accessor` time.
- **Compatibility:** Internal. Some user-visible reflection changes
  (`Object.getOwnPropertyDescriptor`).
- **Stacking:** Strict superset of S1's mechanics; S5 becomes redundant.

### S7 — Skip the parallel ALS ctx; expose ctx fields via a single Symbol-keyed extension on the existing third arg
**Mechanism.** Today there are TWO ctx objects: the singleton third-arg
`ctx` (`runtime.rs:1259-1289`, `{waitUntil, passThroughOnException}`,
frozen, pre-built) and the ALS-installed parallel ctx (`dispatch.rs:79-159`).
Merge them: ship a per-isolate templated `ctx` that's reused across
requests; per-request, store the request-specific data in V8 internal
fields keyed by `Symbol.for("zs:RpcContext")`. `__zeroshipGetRpcCtx()`
becomes a `Symbol` lookup on `globalThis` or the function's `this`.
- **Speedup:** **~50 %.** Eliminates ALS install/restore (~300 ns – 1 µs)
  and the `Object.freeze` on the ctx wrapper.
- **Effort:** ~200 LOC. Complexity 4/5.
- **Risk:** Loses ALS propagation across `setTimeout`/`fetch`-then-await
  hops. The proposal §3 explicitly requires that propagation. Workaround:
  keep ALS for the deep-call case but populate it lazily (from the
  third-arg ctx). Effectively a hybrid of S1 and this.
- **Compatibility:** Same proposal contract.
- **Stacking:** Mutually exclusive with S1's approach.

### S8 — Conditionally install ALS only when user code holds an `AsyncLocalStorage` reference
**Mechanism.** Detect at module-init whether the user's bundle imports
`AsyncLocalStorage` (via the synthetic entry transform or Rust-side
module-graph walk). If not, skip `with_rpc_context_in_als` entirely;
the embedder-data slot is left as undefined. `__zeroshipGetRpcCtx()`
falls back to a per-isolate request-scoped slot that's populated by
`call_fetch_handler` directly (no Map clone).
- **Speedup:** **~10–15 %.** Removes the Map clone+set+restore (~300 ns).
- **Effort:** ~150 LOC. Complexity 3/5.
- **Risk:** Wrong if the user's procedure dynamically imports
  `node:async_hooks` post-module-init. Must mark "uncertain" → keep ALS.
- **Compatibility:** Vite-plugin coordination required for static detection.
- **Stacking:** Subsumed by S1 (if ctx isn't read, the ALS install never
  happens anyway).

### S9 — Slot-affined ctx cache: reset fields between requests on the same isolate
**Mechanism.** Each isolate keeps a single `RpcContextHandle` Global. On
each request, in Rust, write the new String values into the wrapper's
internal fields (request_id, trace_id, method, url, headers raw bytes,
user_json) and re-emit the v8::Local. The Headers/URL/AbortController
sub-objects are preserved across requests but their internal Box state
is reset. The `Object.freeze` and shadow installs happen *once per
isolate*, not per request.
- **Speedup:** **~70 %.** Cleanest version of S5 — only paying for
  state writes, not V8 allocs.
- **Effort:** ~500 LOC; major refactor of Headers/URL/AbortSignal to
  support reset. Complexity 5/5.
- **Risk:** Same hidden-class concerns as S5 plus the AbortController
  identity issue: each request needs *its own* signal because eviction
  must abort *its* in-flight tasks, not the next request's. Either ship
  an `AbortSignal.any` proxy (the current signal slot points at a fresh
  child signal each request) or accept the constraint that the signal
  is the only piece that's still per-request.
- **Compatibility:** Internal.
- **Stacking:** Composes with S1 (signal stays lazy). Probably the
  ultimate target if profiling shows the V8 alloc cost dominates after S1.

### S10 — Re-architect: ctx becomes a Rust handle exposed via a single Symbol with native accessors (V8 "Holder" pattern)
**Mechanism.** No JS object. `__zeroshipGetRpcCtx()` returns a special
host-defined object whose every property is a native getter into a
per-request Rust struct. Headers/URL are minted on-access. AbortController
is minted on-access (with stable identity via the holder's internal
field). Closest to Cloudflare Workers' approach.
- **Speedup:** **~85–95 %.** Pure-Rust ctx state, V8 only allocates
  on-demand.
- **Effort:** ~700 LOC. Complexity 5/5. Touches `runtime-macros` to
  emit a `Holder` v8_class with all 8+ accessors.
- **Risk:** `JSON.stringify(ctx)` now serializes only enumerable
  accessors — semantic change. Mitigated by making accessors
  `enumerable: true`.
- **Compatibility:** Subtle behavior change for `Object.keys(ctx)` which
  walks own properties: with native accessors on prototype, `keys` is
  empty. Add an `ownKeys` Proxy trap or use own-property accessors.
- **Stacking:** Strict superset of all earlier solutions; once shipped,
  S1/S5/S6/S9 are moot.

---

### Speedup recap (estimated)

| Solution | Effort | Speedup | Risk | Stacks with |
| -------- | -----: | ------: | ---: | ----------- |
| S1 — lazy materialize | 2 | 70–85 % | low | S3, S6, S9 |
| S2 — vite static analysis | 4 | 80 % | medium | redundant w/ S1 |
| S3 — drop redundant freeze + per-setter shadows | 2 | 15–25 % | low | all |
| S4 — Map-shaped headers | 3 | 15–20 % | medium | redundant w/ S1 |
| S5 — pooled wrappers | 5 | 50–60 % | high | S1 |
| S6 — ObjectTemplate accessors | 4 | 40–60 % | medium | S1 |
| S7 — merge ALS ctx with singleton | 4 | 50 % | medium | excludes S1 |
| S8 — conditional ALS install | 3 | 10–15 % | low | subsumed by S1 |
| S9 — slot-affined reset cache | 5 | 70 % | high | S1 |
| S10 — Rust-handle holder | 5 | 85–95 % | medium | supersedes S1/S5/S6/S9 |

---

## Section 5 — Recommended sequencing

**Phase 1 (quick wins, ≥ 80 % recovery, ~250 LOC total):**

1. **S1 — lazy ctx via materialize-on-first-`__zeroshipGetRpcCtx()`.** This
   single change, by itself, recovers most of the regression on the bench
   (which never reads ctx) and on most early-adoption workloads (where
   ctx is used at most once per call). Build the lightweight Rust
   `RpcContext` always (cheap), defer V8 allocation until first read.
2. **S3 — drop the redundant `Object.freeze` callouts on Headers, and
   drop the per-setter shadow installs on URL.** Keep the existing
   `Immutable` guard for Headers; add an equivalent guard slot for URL
   (one boolean checked by setter callbacks). Eliminates ~14
   `define_property` calls and 3 freeze callouts per request.

If S1 + S3 ship and the macrobench recovers to >900K req/s, we're done.

**Phase 2 (strategic, only if Phase 1 doesn't close the gap):**

3. **S6 or S9 — ObjectTemplate accessors or slot-affined reset cache.**
   Pick based on profiling: if remaining cost is in the property writes
   on the ctx wrapper, S6 wins. If it's in Headers/URL allocation cost
   on procedures that DO read ctx, S9 wins.

**Phase 3 (only if Phase 2 still leaves >5 % gap):**

4. **S10 — Rust-handle holder.** Strategic; treats `ctx` as a native
   class. Requires proposal amendment (own-property visibility semantics).

**Solutions to skip / deprioritize:**

- **S2** — redundant with S1; S1's runtime overhead (~50 ns to detect
  unused-ctx) is cheaper than the build-time tooling investment for S2.
- **S4** — semantic change for marginal gain; S1 dominates.
- **S5** — superseded by S9 (which is cleaner) and by S1 (which removes
  the need entirely for the unused-ctx case).
- **S7** — abandons ALS propagation, breaks proposal §3.
- **S8** — small win; subsumed by S1.

---

## Section 6 — Open questions

These need a profiler run or microbench before committing to a path.

1. **Where exactly does the 4 µs go?** The §2 breakdown is estimate-only.
   A targeted `perf record` over the macrobench (`v8-16w / ping`) plus
   `flamegraph` on the worker thread would resolve which sub-cost
   dominates. Specifically:
   - Is `Object.freeze` showing up as JS-call frames (`Builtins::Object_freeze`)?
   - Is `ada::Url::parse` showing up?
   - Is `clone_map` showing up at all? (Likely not, given empty user ALS.)
   - Is `Function::new` for the throwing-fn callback showing up?
2. **How much of the 81 % loss is GC pressure vs per-call latency?** A
   run of the macrobench with `--trace-gc-verbose` (or compio's idle GC
   knob set to never) would distinguish. If GC is dominant, S1 alone
   may not be enough — S5/S9 also reduce alloc rate.
3. **Does the dispatch microbench fail to detect the regression because
   of `app_id`-skipping?** Re-running `rpc_dispatch.rs` with
   `Runtime::builder().app_id(Uuid::new_v4()).build()` would reveal the
   AbortGuard cost. Worth re-running before/after any fix.
4. **Is the URL parse cost amortized via ada-url's interning?** ada-url
   does no interning for parsed URLs — each parse builds a fresh state
   machine pass. Confirmation: a microbench of `build_native_url` alone
   (skip ALS, skip Headers).
5. **What is the actual cost of `with_rpc_context_in_als` when the map
   is empty?** The fast path (`als.rs:281-285`) returns `None`, falling
   through to `v8::Map::new(scope)`. Is the `set_continuation_preserved_embedder_data`
   itself measurable, or is the cost the snapshot Global creation?
6. **What's the V8 hidden-class deopt rate on the URL wrapper?** Each
   request installs 14 own-property descriptors on a fresh URL via
   `define_property`. V8's IC for the user's `ctx.url.pathname` access
   may be deoptimizing on every call. `--trace-ic` would reveal.
7. **Was there any other code change between `aab1153` (last good
   pre-bisect commit) and `9be897d` that might contribute?** The bisect
   identified `9be897d` definitively but there may have been parallel
   landing of synthetic-entry changes (`docs/proposals/rpc-v2.md` §6's
   `_zsRpc` slow path) that re-invoke the procedure for AsyncIterator.
   Confirm via `git log 9be897d~5..9be897d -- crates/runtime crates/worker`.
8. **Does setting `app_id = None` on the worker (i.e. skipping
   `register_in_flight`) recover throughput?** A simple A/B switch in
   `worker/src/main.rs` would isolate the AbortGuard cost. If it's
   measurable on its own, S1 must explicitly preserve the eager registry
   call (which is only ~150 ns and necessary for eviction abort).
9. **Could the regression be partly due to `per_request_user.get(&request_id).cloned()`
   for the empty-anon case?** `runtime.rs:1550` borrows `state` and
   probes a HashMap on every request even when there's no auth. A
   pre-check (`if state.borrow().per_request_user.is_empty()`) might
   shave 50 ns.
10. **What's the right target throughput?** The pre-regression 1162K req/s
    @ 16w is the bar to clear. If S1+S3 land at ~1050K, is that
    acceptable, or does the proposal commit us to fully recovering the
    headline number? The platform's `200K req/s` claim in
    `docs/decisions/2026-04-20-kernel-cut.md` — well below the 1162K — suggests
    the bar is the platform's published number, not the highest historical
    measurement.
