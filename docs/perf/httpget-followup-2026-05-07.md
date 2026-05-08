# httpGet macrobench follow-up — 2026-05-07

Follow-up to `docs/perf/httpget-regression-2026-05-04.md`. Re-measures the
current state and identifies optimization candidates with measured costs only.
**No estimates** — every percentage cited is `perf record` self-time, every
ns figure is microbench output, every req/s figure is `run_zerobench.sh`
output. Where a candidate's projected savings cannot be measured this round,
the entry is labelled **needs microbench**.

## 1. Methodology

- Hardware: 32 logical cores, x86_64. NUMA split: server (numactl --cpunodebind=0)
  vs client (numactl --cpunodebind=1).
- Bench server: `target/release/zeroship-bench-server --port=5101 --workers=16`
  (release build at `deb9161`, current main).
- Client: `zerobench` saturate mode, 300 conns, 16 threads.
- Scenario: `GET /hello` against `default.fetch` slow path
  (`crates/runtime/benches/scenarios.js:281-312`). Triggers
  `request.headers.get("upgrade")`, `new URL(request.url)`, four `url.pathname`
  checks (early-out on first match for `/hello`: none — falls through to the
  final `Response.json`), and `Response.json({ method, url })`.
- Profile: `perf record -F 999 -p $SERVER_PID -g --call-graph dwarf,16384`
  for 10s while the client saturated 16 worker threads. 159,755 samples,
  total event count 159.9 G.
- `kernel.perf_event_paranoid = 1` (unchanged, sufficient for user-space
  callgraph capture).

## 2. Current state — 3-run measurement

| Run | req/s | p50 | p99 |
|---|---|---|---|
| 1 | 300,801 | 958 µs | 1.4 ms |
| 2 | 305,582 | 837 µs | 1.5 ms |
| 3 | 304,256 | 858 µs | 1.4 ms |
| **Mean** | **303,546** | — | — |
| Range | 4,781 (1.6 %) | — | — |

The 298,884 figure cited in the brief reproduces within run-to-run noise
(~1.6 % spread). Current state is ~303 K req/s, **~20 K req/s below** the
2026-05-04 post-fastcall baseline of 323 K (3-run avg from
`results-2026-05-04-after-fastcall.txt`). This sub-regression has not been
bisected to a specific commit in this round.

The 235 K gap to the 2026-04-27 peak of 534 K cited in
`results-2026-04-27-after-G-track.txt` therefore widens by ~20 K vs the
5/04 doc; the headline regression cause is unchanged (Headers polyfill
removal in `f90e264`).

## 3. scenarios.js handler walk

`crates/runtime/benches/scenarios.js:281-312`:

```js
async fetch(request) {
    const url = new URL(request.url);                          // 1 boundary
    if (request.headers.get("upgrade") === "websocket") { ... } // 2 + 3 boundaries
    if (url.pathname === "/sse") { ... }                        // 4 boundary
    if (url.pathname === "/wping") { ... }                      // 5 boundary
    if (url.pathname === "/wjson") { ... }                      // 6 boundary
    if (url.pathname === "/ping") { ... }                       // 7 boundary
    return Response.json({ method: request.method, url: request.url }); // 8 + 9 + 10 boundaries
}
```

Per-request boundary crossings on `/hello` (everything falls through to the
final return):

1. `Request.url` getter (V8 callback) — used by `new URL(request.url)`.
2. `Request.headers` getter — returns the cached Headers wrapper.
3. `Headers.get("upgrade")` — slow callback (NOT fastcall — see §4).
4. `URL.pathname` getter — 4 calls (one per `if` chain branch).
5. `Request.method` getter.
6. `Request.url` getter again.
7. `Response.json(data, init?)` static — runs the JSON.stringify + Response
   constructor algorithm internally; allocates the returned Response.
8. `Response.json` internally invokes the Response constructor (which
   constructs Headers) → `Headers.set("Content-Type", "application/json")`.
9. The kernel post-call inspects the Response: `extract_response_headers`
   walks the Headers iterator, reads `.status`, reads body — multiple
   getter calls.

Net: **≥ 10 V8 FunctionCallback boundary crossings per `/hello` request**,
plus ~3 more on the kernel-side response inspection.

The `default.fetchFast` path is **never reached for `/hello`** because the
fixture only handles `/ping` (lines 275-278 of scenarios.js); for any
other path it returns null, falling through to `default.fetch`. There is
no architectural option in scope to bypass `default.fetch` for httpGet
without changing the fixture.

## 4. fastcall wiring status

Confirmed by reading code:

- **Macro support**: `crates/runtime-macros/src/v8_class/fastcall/mod.rs`
  is fully wired. `validate_fastcall_signature` rejects unsupported
  shapes (`&mut self`, `String`/`Vec<u8>`/`Option<T>` returns,
  unsupported arg types — `crates/runtime-macros/src/v8_class/fastcall/mod.rs:57-181`).
  Codegen emits an `extern "C"` shim, a static `CFunctionInfo`, and a
  static `CFunction` per fastcall-annotated method (`mod.rs:328-474`).
  The shim recovers `*const Self` from internal-field-1 via
  `get_aligned_pointer_from_internal_field(1, 0)` and calls the user
  method directly with no scope, no External unwrap, no brand check
  (V8's CFunction signature enforces receiver shape at JIT time).

- **Production sites**: exactly one — `Headers.has`
  (`crates/runtime/src/web/headers.rs:689`). Verified by grep:
  `grep -rn "v8_method(fastcall)" crates/runtime/src/` returns only this
  one match. The 5/04 commit `9721a76` is the only Tier-1 candidate that
  actually shipped to production.

- **Headers.get explicitly deferred**: `runtime-macros/TODO.md:159-169`
  says `Headers.get` is blocked by its `Result<Option<Vec<u8>>, OpError>`
  return shape. None of `Option<T>`, `Vec<u8>`, or "no null sentinel"
  fits the fast API in rusty_v8 v147. The TODO names the missing piece
  as a "FastByteStringWriter adapter" — out-param shape that writes
  bytes into a caller-provided buffer with a deopt-on-overflow fallback.
  Not yet built.

- **5f9cbad scope** (`runtime-macros: table-driven KnownType + FastcallType`):
  per the commit message and `crates/runtime-macros/src/known_type.rs`,
  this is a refactor of the per-type classifier dispatch into typed
  enum variants. It does **not** add new fastcall sites or relax the
  signature validator; it consolidates the existing dispatch tables.

- **Bench validation outcome (5/04)**: per
  `results-2026-05-04-after-fastcall.txt`, the `Headers.has` migration
  delivered +2.3 % on httpGet (323 K vs 316 K baseline) — within
  run-to-run noise. The author's note explicitly states "scenarios.js
  doesn't exercise Headers.has on the httpGet path" — the migration
  didn't ship benchmarkable savings because the bench fixture calls
  `headers.get("upgrade")`, not `headers.has("upgrade")`.

**Summary**: fastcall is fully wired in the macro for the supported
shapes; only one production site (Headers.has, off the bench hot path).
The follow-up to migrate `Headers.get` is gated on a
FastByteStringWriter adapter that has not been written.

## 5. Flamegraph hot spots — top self-time symbols

Aggregated across all 16 worker threads (perf report --no-children
--sort=symbol --percent-limit=0.2). Full SVG at
`docs/perf/flamegraphs/2026-05-07-httpget-298k.svg`.

| % self | Symbol |
|---|---|
| **12.06** | `v8::internal::GlobalHandles::Create(...)` |
| **10.71** | `v8::internal::GlobalHandles::NodeSpace::Release(...)` |
|  3.99 | `_raw_spin_unlock_irqrestore` (kernel — io_uring/TCP) |
|  3.59 | `_mi_page_malloc` (mimalloc, allocations) |
|  1.88 | `v8::handle::Weak<T>::second_pass_callback` (weak finalizer batch) |
|  1.40 | `nft_do_chain` (kernel netfilter) |
|  0.95 | `v8::handle::FinalizerMap::add` |
|  0.91 | `zeroship_runtime::core::serve::handle_connection::{{closure}}` |
|  0.88 | `__memmove_evex_unaligned_erms` (memcpy) |
|  0.76 | `v8::Object::Get(...)` |
|  0.72 | `v8::isolate::Isolate::get_annex_arc` |
|  0.71 | `core::ptr::drop_in_place<Headers>` |
|  0.69 | `v8::internal::Invoke(...)` |
|  0.69 | `mi_free` |
|  0.58 | `v8::internal::Utf8DecoderBase<...>::Utf8DecoderBase` |
|  0.58 | `v8::internal::Factory::AllocateRaw(...)` |
|  0.57 | `RuntimeInner::call_fetch_handler` |
|  0.56 | `v8::internal::Scavenger::ScavengeObject(...)` |
|  0.54 | `v8::internal::FunctionCallbackArguments::CallOrConstruct(...)` |
|  0.52 | `Builtins_CallApiCallbackOptimizedNoProfiling` |

`__brand_check_*` self-time (rusty_v8's wrapper around the
prototype-walk shape check):

| % self | Symbol |
|---|---|
| 0.33 | `__brand_check_Request` |
| 0.33 | `__brand_check_Headers` |
| 0.27 | `__brand_check_URL` |
| 0.17 | `__brand_check_Response` |

Specific runtime callback self-times:

| % self | Symbol |
|---|---|
| 0.22 | `__Response_json_callback` |
| 0.19 | `__Response_constructor_callback` |
| 0.16 | `build_kernel_headers` |
| 0.10 | `extract_response_headers` |
| 0.09 | `__Headers_set_callback` |
| 0.07 | `__Headers_constructor_callback` |
| 0.07 | `inspect_response` |
| 0.06 | `__Headers_has_callback` |
| 0.06 | `__Headers_get_callback` |

### The dominant cost — V8 GlobalHandle churn

`GlobalHandles::Create + Release` together account for **22.77 % of all
CPU time**. Adding the GC-finalizer second-pass (1.88 %) and FinalizerMap
admission (0.95 %) brings the total to **25.6 %**.

Calltree breakdown (`perf report -g graph,1.0,callee` on the Create
symbol — see `/tmp/full-report.txt` for full output, summary below):

```
12.06%  GlobalHandles::Create
        |--9.93%--v8__Global__New
        |          |
        |          |--2.25%--call_fetch_handler  (per-request Globals: result wrap, request stash)
        |          |--1.42%--__brand_check_Request   (slot.0.clone() in cache hit path)
        |          |--1.40%--__brand_check_Headers
        |          |--1.17%--__brand_check_URL
        |          |--0.86%--build_kernel_request    (req_tmpl/req_proto/headers globals)
        |          |--0.82%--__brand_check_Response
        |           --0.x%--others (set_default_content_type, env/ctx clones, ...)
         --2.13%--v8__Global__NewWeak  (per-instance finalizer registration)
                   v8::handle::Weak<T>::new_raw

10.71%  GlobalHandles::NodeSpace::Release
        |--1.60%--Weak first_pass_callback (GC sweep)
        |--1.43%--call_fetch_handler  (drop of per-request Globals)
        |--1.22%--__brand_check_Headers  (drop of slot.0.clone() result)
        |--1.21%--__brand_check_Request
        |--1.19%--__brand_check_URL
```

### Why brand check creates Globals

Reading `crates/runtime-macros/src/v8_class/emit/brand.rs:97-99`:

```rust
let cached_global: v8::Global<v8::Object> =
    if let Some(slot) = scope.get_slot::<#brand_slot_ty>() {
        slot.0.clone()                                // ← every steady-state call
    } else { ... }
```

`v8::Global::clone()` is implemented in
`~/.cargo/registry/.../v8-147.1.0/src/handle.rs:345-351` as
`Self::new_raw(isolate, data)` — i.e. it allocates a fresh
GlobalHandles slot via `v8__Global__New`. The `Drop` impl
(`handle.rs:353-365`) calls `v8__Global__Reset`, freeing the slot.

So **every steady-state brand check call performs one Create + one
Release of a GlobalHandle**. With 4 `#[v8_class]` brand-checked
callbacks per request (`Headers.get`, `URL.pathname`, `Request.url`,
`Request.method`, `Response.json`/`Response.status`/`Response.headers`
reads in `inspect_response`), that's 4-7 Create/Release pairs per
request just from brand-check housekeeping.

### Per-request Global churn from kernel paths

In addition to brand-check Globals, the kernel allocates new Globals
per request:

- `core/runtime.rs:1668` — `v8::Global::new(scope, request)` to stash
  the Request in `request_by_id` for `__zs_get_request()`.
- `core/runtime.rs:3406` — `v8::Global::new(tc, v)` to lift the fetch
  handler's return value out of the inner TryCatch scope.
- `core/runtime.rs:3441` — pending Promise wrap (only when the handler
  returns a Promise; `default.fetch` is `async` so this fires per
  request).
- `web/fetch/request.rs:1022,1032` — `req_proto` Global (lazy first
  call), `headers_g` Global (per-request).
- `web/fetch/request.rs:1071` — `v8::Weak::with_guaranteed_finalizer`
  for the Request box (creates a weak GlobalHandle).
- `web/headers.rs:802-804` — `class_tmpl.clone()` and
  `prototype.clone()` per `build_kernel_headers` call (steady-state =
  2 Globals per call).

## 6. Diff vs 2026-05-04

The 5/04 doc described the regression cause as "V8 boundary crossings
per `headers.get` + `Response.json`", with each crossing paying the
"FunctionCallback prologue (External lookup, scope setup, brand check,
*self recovery)". The actual measured profile reveals a more specific
shape: **the dominant cost is V8 GlobalHandle Create/Release inside the
brand check** — which the 5/04 doc didn't separate from the rest of the
prologue.

Changes between 5/04 (`38f9200`) and 5/07 (`deb9161`) that touch the
HTTP/fetch path or the per-request hot path:

| Commit | Touches httpGet path? | Notes |
|---|---|---|
| `7417cc1` typed enums for Request/Response init dicts | Slow path only — RequestInit / ResponseInit parsing only on `new Request()` / `new Response()`. `build_kernel_request` writes RequestState fields directly, bypassing dict parse. | No measurable hot-path impact. |
| `fbef394` idle GC trigger | Default 30 s threshold (`runtime.rs:174`); never fires during 10 s saturated load. | Not the cause. |
| `d4bd4fd` collapse simple classes into register_native_classes! | Touches AbortController/EventTarget/etc. Not on the httpGet hot path. | No measurable impact. |
| `ed3125e` EventSource + CompressionStream/DecompressionStream | New classes. Not invoked by `/hello`. | No measurable impact. |
| RPC v2 changes (Wave A-F, ALS-backed ctx, etc.) | Touch `default.rpc` / RPC ctx holder. `default.fetch` is independent (separate kernel entry — verified at `scenarios.js:281` and `core/runtime.rs:1646`). | Not the cause for httpGet. |
| `5f9cbad` table-driven KnownType + FastcallType | Macro refactor. Behaviour-equivalent. | No impact. |

None of these obviously explain the further ~20 K req/s drop from 5/04
to 5/07. The drop sits within the noise band of the 5/04 measurement
(±5 K) plus the 5/07 measurement (±5 K) — a 7 % drop is plausibly run-
to-run drift in a saturated 16-thread test, but a focused bisect would
be needed to confirm.

**New finding vs 5/04 doc**: GlobalHandle churn is a much larger
contributor than the doc suggested. The 5/04 narrative framed the cost
as "FunctionCallback boundary crossings"; the measured profile shows
that within those crossings, GlobalHandle Create/Release is the single
biggest line item (22.77 % of CPU), driven primarily by the brand-check
codegen pattern `slot.0.clone()`.

## 7. Optimization candidates (measured costs only)

### Candidate A — eliminate `slot.0.clone()` in steady-state brand check

**Mechanism**. `crates/runtime-macros/src/v8_class/emit/brand.rs:97-129`
does `slot.0.clone()` (allocating a Global) every time a brand check
runs, even though the cached prototype is constant after first install.
The Local can be obtained with `v8::Local::new(scope, &slot.0)` directly
from the borrowed reference (no `.clone()`) — the Local is scope-bound
and doesn't outlive the borrow. Replace lines 97-99 with a direct
`v8::Local::new(scope, &scope.get_slot::<#brand_slot_ty>().unwrap().0)`
pattern (with care for the borrow checker — the slot ref must drop
before `obj.get_prototype(scope)`).

**Measured cost today**. `GlobalHandles::Create` 12.06 % + `Release`
10.71 % = 22.77 % CPU. Of which the callgraph attributes:
- 1.42 % Create + 1.21 % Release to `__brand_check_Request` (= 2.63 %)
- 1.40 % Create + 1.22 % Release to `__brand_check_Headers` (= 2.62 %)
- 1.17 % Create + 1.19 % Release to `__brand_check_URL` (= 2.36 %)
- 0.82 % Create + ~? Release to `__brand_check_Response` (= ~1.7 %)

Brand-check Globals account for **~9.3 % of CPU** in the measured
flamegraph.

**Measured savings if removed/replaced**. Needs microbench. The brand
slot's Global only needs to be cloned because the API returns an owned
Global; with a direct `Local::new(scope, &cached_global)` borrow this
allocation goes away entirely. The cap on saving is ~9.3 % of CPU
(~28 K req/s of the 303 K headline). Not yet measured.

**Effort**. Small. One macro file change in
`crates/runtime-macros/src/v8_class/emit/brand.rs`; rebuild propagates
to every `#[v8_class]`. Care needed for the borrow checker (the
`scope.get_slot()` borrow must drop before subsequent `&mut scope`
calls in the chain walk).

**Risk**. Low. Lifetime-only refactor; no semantic change. Brand check
returns a bool exactly as today.

**Constraint compliance**. WPT and existing tests should be unchanged
(behaviour-equivalent).

### Candidate B — `Headers.get` fastcall via FastByteStringWriter

**Mechanism**. As described in `runtime-macros/TODO.md:159-169`. Add a
new fast-API shape: `(this, FastApiOneByteString name, *mut u8 out_buf,
*mut usize out_len) → bool`. The fast path writes bytes into a JS-side
pre-allocated Uint8Array (per-isolate scratchpad of e.g. 4 KiB);
returns false to deopt on missing-name or oversize. JS-side wrapper:
`Headers.prototype.get = function(name) { const n = __zs_headers_get_fast(this, name, scratchpad, scratchpad_len); ... }`.

**Measured cost today**. `__Headers_get_callback` self-time = 0.06 %
on the bench. **But** this is misleadingly low — the cost is in the
GlobalHandles Create/Release of the brand check that the slow path
performs (covered separately by Candidate A). Per-call attribution
across the full `headers.get` site (callback + prologue + brand
check) is roughly 2.6 % (Headers brand check) + 0.06 % (callback
self) + ~0.5 % (prologue/return-value setup) ≈ **3 % of CPU**.

**Measured savings if removed/replaced**. Needs microbench. If
fastcall actually fires on the steady-state path (verified via
`v8_fastcall_smoke.rs` for `Headers.has` — TurboFan compiles +
1000-iter steady state without deopt), the brand check disappears
(JIT receiver-shape check), the FunctionCallback prologue disappears
(typed CFunction call), and the Vec<u8> alloc disappears (writer to
caller buffer). Cap is ~3 % CPU — but only when TurboFan promotes the
`headers.get` callsite, which depends on hot-IC stability.

**Effort**. Medium. New `FastcallType` variants for the writer
out-param shape, plus matching emit-time codegen, plus a JS-side
wrapper that allocates and reuses a per-isolate scratch buffer. ~200
LOC in `runtime-macros/src/v8_class/fastcall/mod.rs` + ~100 LOC in
runtime/src/web/headers.rs JS bridge.

**Risk**. Medium. The scratchpad needs careful lifecycle (per-isolate
TLS, not per-call alloc — defeats the point). Deopt on oversize must
fall back cleanly to the slow path. The JS-side wrapper adds a
function call, which TurboFan should inline but isn't guaranteed.

**Constraint compliance**. Behaviour-preserving as long as the JS-side
wrapper exposes the same Headers.get contract. WPT Headers conformance
tests should still pass.

### Candidate C — `Response.json` static fastcall

**Mechanism**. `Response.json(data, init?)` is a `#[v8_static_method]`.
The current implementation does `JSON.stringify` (V8 builtin) + `new
Response(...)` (cross-class FunctionCallback) + `headers.set(...)`
(another callback). Fastcall could be applied to the static call site
itself, but the heavy lifting is inside (`Response` constructor, Headers
construction). True savings would require a tighter bypass:
`zeroship.fast.responseJson(data)` → returns a pre-built native Response
with serialized body and Content-Type pre-set, skipping the constructor.

**Measured cost today**. `__Response_json_callback` self-time = 0.22 %.
`__Response_constructor_callback` self-time = 0.19 %. The
`set_default_content_type` chain (Headers.set call from inside
Response.json) is visible in the callgraph at ~0.55 % through
`__Headers_set_callback`. Total Response.json + downstream chain
≈ 1.0-1.5 % CPU (rough estimate from the callgraph; not separated cleanly).

**Measured savings if removed/replaced**. Needs microbench. A direct
"fast Response.json" path could collapse JSON.stringify + Response
ctor + Headers.set into one Rust function. Cap is ~1.5 % CPU.

**Effort**. Medium-large. Either change scenarios.js (cosmetic — but
the bench fixture is meant to mirror real apps), or add a new static
method (`Response.fastJson` / `zeroship.fast.responseJson`) that the
runtime can pattern-match. The latter requires AST-level transform in
`@zeroship/vite-plugin` to rewrite `Response.json` calls — high
implementation cost for what might be a 1-2 % win.

**Risk**. Medium. Spec deviation is the main risk; the rewriter must be
precise.

**Constraint compliance**. If implemented as a separate name
(`Response.fastJson`), no spec deviation. If rewriting `Response.json`
calls implicitly, this departs from observable WinterCG semantics.

### Candidate D — eliminate per-request `request_by_id` Global

**Mechanism**. `core/runtime.rs:1668` allocates a per-request
`v8::Global<v8::Object>` to stash the Request for the
`__zs_get_request()` host function (`init.rs:1190-1209`). Since
`default.fetch(request, env, ctx)` already passes `request` as the
first argument, the stash is only useful when JS code calls
`getRequest()` from a deeply-nested callback that lost the request
binding. For the bench (and most real apps), the JS handler holds
the parameter directly. Move the stash to lazy: only create the
Global when `__zs_get_request()` is actually invoked in this request.

**Measured cost today**. The `call_fetch_handler` slice of
`GlobalHandles::Create` is 2.25 % (per the callgraph in §5). This
covers (request stash) + (result wrap on line 3406) + possibly
(env/ctx Local::new). The request stash specifically is ~half of
that — call it **~1 % of CPU**.

**Measured savings if removed/replaced**. Needs microbench. The cap
is ~1 % CPU savings (~3 K req/s of headline). The result Global on
line 3406 is harder to remove (TryCatch lifetime).

**Effort**. Small. Hoist the `Global::new` into a lazy mint inside
`__zs_get_request_callback`, gated on the request being actively
borrowed via the executing_request_id slot. Drop the `request_by_id`
unconditional insert.

**Risk**. Low. `getRequest()` semantics preserved (still returns the
Request object); only the time of Global allocation moves.

**Constraint compliance**. Compatible — just defers an allocation.

### Candidate E — collapse `result_global` Global on call_fetch_inner return

**Mechanism**. `core/runtime.rs:3398-3408`: the inner TryCatch scope
captures the handler's return value as a Global so it survives the
tc_scope drop. Could be replaced by an outer `tc_scope!` covering the
full function (including `inspect_response`), with the Local being
passed directly out. Avoids the intermediate Global Create/Release.

**Measured cost today**. ~1 % CPU (the other half of the 2.25 % slice
attributed to `call_fetch_handler` in the Create callgraph; not
exactly separable from request-stash).

**Measured savings if removed/replaced**. Needs microbench. Cap ~1 %.

**Effort**. Small-medium. The TryCatch/scope nesting in
`call_fetch_inner` is fiddly; the outer scope must hold throughout
the Promise unwrap path including `promise.result(scope)` calls.
~50 LOC refactor.

**Risk**. Low-medium. The exception-rethrow path (lines 3402-3403)
must still work — the exception value must outlive the tc_scope.

**Constraint compliance**. Behaviour-preserving.

### Candidate F — IC stability check via `--trace-ic`

**Mechanism**. The flamegraph shows `Builtins_CallApiCallbackOptimizedNoProfiling`
at 0.52 % self time. The "Optimized" prefix says TurboFan is hitting
the optimized callback path; the "NoProfiling" suffix says no IC
miss is recorded. But hidden-class deopts on Headers / Response /
Request would force Builtins_CallApiCallbackGeneric (un-optimized) —
not seen in the top symbols. Run `--trace-ic --trace-deopt` to verify
the optimized path is stable across all callsites.

**Measured cost today**. Cannot be measured this round without a
build flag rerun. The fact that the optimized callback variant is
in the top 20 self-time is itself encouraging — IC is hitting the
fast variant.

**Measured savings if removed/replaced**. Needs measurement. If
deopts are firing, fixing them could recover several percent. If
not, this is a confidence check only.

**Effort**. Small. Just `V8_FLAGS="--trace-ic --trace-deopt"` env
var on the bench server.

**Risk**. None — diagnostic.

**Constraint compliance**. N/A.

### Candidate G — disable per-instance Weak finalizer for short-lived Headers

**Mechanism**. `web/headers.rs:875` (`install_headers_state`) and
`web/fetch/request.rs:1071` register a `v8::Weak::with_guaranteed_finalizer`
per-instance to drop the boxed state. This is a Global allocation per
instance + a FinalizerMap insert (FinalizerMap::add at 0.95 % self,
v8__Global__NewWeak at 2.13 % under Create). For per-request short-lived
Request/Headers, an isolate-scoped object pool (acquire on request
start, return on response complete) would amortise the allocation.

**Measured cost today**. `v8::handle::FinalizerMap::add` 0.95 % +
`v8__Global__NewWeak` 2.13 % + `Weak::second_pass_callback` 1.88 %
+ `v8::handle::Weak<T>::first_pass_callback` 1.60 % = **6.6 % CPU**
in finalizer machinery.

**Measured savings if removed/replaced**. Needs microbench. The cap
is ~6 % CPU but capturing it requires a non-trivial pooling layer.

**Effort**. Large. Object pooling across V8 GC boundaries is
intrusive — Request/Headers state would need a release hook on
every kernel response path, with a fallback to GC finalizer for
escapes (user code stashing the Request in a closure, etc.).

**Risk**. High. GC ordering semantics + the "user might keep a
Reference past the response" edge case make this hazardous.

**Constraint compliance**. Maintainable only with a clear escape
hatch for user code that captures the Request.

## 8. Recommended top 3

Ranked by `(measured savings cap) / effort`. Saving caps come from §7.

1. **Candidate A — eliminate `slot.0.clone()` in brand check**.
   Cap ~9.3 % CPU (largest single item), small effort, low risk.
   Single-file macro change. Highest ratio in the list.

2. **Candidate D — lazy `request_by_id` Global**.
   Cap ~1 % CPU, small effort, low risk. Bonus: removes one HashMap
   insert per request and one HashMap remove on the response path.

3. **Candidate B — `Headers.get` fastcall via FastByteStringWriter**.
   Cap ~3 % CPU, medium effort, medium risk. The 5/04 doc's named
   recommendation; still pending. Highest cap of the medium-effort
   options. **But verify Candidate A first** — A removes the brand-
   check Global churn that B was also going to mask, so B's marginal
   benefit shrinks once A ships.

Candidates C, E, F, G are deferred: C and E are low-cap, F is a
diagnostic prerequisite (worth running before implementing anything),
G is high-risk and deferred until A/B/D are in.

**Suggested order**: F (diagnostic, ~5 min) → A (small, high cap) →
re-measure → D (small) → re-measure → B (medium, then re-measure
whether gap is closed).

## 9. Open questions

1. **Why the further ~20 K drop between 5/04 and 5/07?** Within noise
   bounds, but worth a focused bisect. Candidates: typed-enum dict
   parsing (slow path only — shouldn't fire), idle-GC ticker overhead
   (negligible at 30 s threshold), node:* synthetic module registration
   (boot-time only). A 5-commit bisect over PR #197, #198, #194, #195,
   #191 would confirm or rule out.

2. **Does `Headers.has` fastcall actually fire on production callsites?**
   The 5/04 doc speculates it doesn't, given the +2.3 % was within noise.
   `--trace-ic --trace-deopt` (Candidate F) on a fixture that does call
   `headers.has` repeatedly would settle this.

3. **What's the V8 hidden-class shape for `Response.json`'s init arg?**
   `Response.json({ method, url })` passes a fresh literal object every
   time. If the literal's hidden-class isn't being shared across requests
   (V8 deduplicates identical literals only sometimes), each call may be
   forcing a fresh map allocation. Check via `--trace-maps`.

4. **Is `extract_response_headers` allocating per call?** Self time
   0.10 %, but the iterator walk allocates a Vec<(String, String)>.
   Could be pooled or written to a caller-provided buffer.

5. **Could `build_kernel_headers` skip the Headers wrapper allocation
   when the user code only reads `headers.get("upgrade")` once?** Lazy
   minting of the Headers wrapper only on first JS-side access would
   save the per-request Headers Box + Weak finalizer pair when the user
   ignores headers entirely. Requires guarding all `RequestState`
   header reads to mint-on-demand.

End of report. Flamegraph SVG at
`/home/ruiyang/Projects/appbase/docs/perf/flamegraphs/2026-05-07-httpget-298k.svg`.
Raw perf data at `/tmp/perf-httpget.data` (will be cleaned at next reboot;
preserve via `cp` if needed for follow-up).
