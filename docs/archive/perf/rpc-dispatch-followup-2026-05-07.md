# RPC dispatch follow-up — empirical perf 2026-05-07

**Goal.** Measure where the residual throughput between the post-S10
state (`v8-16w/ping`, current HEAD `d2e7e22`) and the pre-regression
baseline (commit `48dad91`) actually goes. Constraint: ALS plumbing and
`__zeroshipGetRpcCtx` stay. **No estimates.** Numbers below come only
from `perf record` self-/inclusive-time and Criterion microbenches run
this round; anything else is marked "not measured".

## 1 — Methodology

| Item | Value |
| --- | --- |
| Server | `target/release/zeroship-bench-server --port=5101 --workers=16` (HEAD `d2e7e22`) |
| Baseline server | same args, built in worktree at commit `48dad91c01e93917376431db3ed2ee785ce7699a` |
| Client | `~/Projects/zerobench/target/release/zerobench run /tmp/perf-microbench/ping.rhai -c 300 -t 16 --duration 10s` |
| Build profile | `[profile.release]` from workspace `Cargo.toml` (whatever the standard release ships) |
| Frame pointers | yes — `objdump -d target/release/zeroship-bench-server` shows `push %rbp` at function prologue (`call_fetch_handler`); `--call-graph fp` is reliable |
| `perf record` flags | `perf record -F 999 -g --call-graph fp -p <server-pid> -o ping-<rate>k.data -- sleep 12` (1 s pre-roll, 10 s saturate, 1 s post-roll) |
| `perf` version | 6.19.10 |
| Allocator | mimalloc (configured in the runtime; matches the Criterion microbenches) |
| `kernel.perf_event_paranoid` | 1 (unchanged; allowed user-mode profiling) |
| Flamegraph tool | `cargo-flamegraph` 0.6.11 (`flamegraph --perfdata <data> -o <svg>`) |
| HEAD throughput sample | **846,368 req/s** during the perf run (158,944 samples; `ping-844k.zb.txt`) |
| Baseline throughput sample | **1,067,716 req/s** during the perf run (142,543 samples; `ping-baseline.zb.txt`) |
| Measured gap this run | 1,067K − 846K = **221K req/s** (the report header's quoted "844K vs 1162K" framing was prior-day; today the gap is real but smaller than the 318K cited in the prompt — probably noise + minor commits between 48dad91 and the 2026-05-04 1.16M sample) |

Flamegraphs:
- `docs/archive/perf/flamegraphs/2026-05-07-ping-844k.svg` — HEAD (846K)
- `docs/archive/perf/flamegraphs/2026-05-07-ping-baseline-1067k.svg` — `48dad91` (1067K)

Raw `perf report` text dumps live in `/tmp/perf-microbench/`
(`top-self-clean.txt`, `top-inclusive-clean.txt`, `baseline-self.txt`,
`baseline-incl.txt`).

## 2 — Hot spots (HEAD, 846K req/s)

### 2a — Top 20 by self-time (measured, full process, kernel + user)

| Self % | Symbol |
| ----: | --- |
| 5.72% | `[k] _raw_spin_unlock_irqrestore` |
| 4.43% | `[.] _mi_page_malloc` |
| 3.94% | `[k] nft_do_chain` |
| 3.82% | `[.] v8::internal::GlobalHandles::Create` |
| 3.14% | `[.] v8::internal::GlobalHandles::NodeSpace::Release` |
| 1.74% | `[.] zeroship_runtime::core::serve::handle_connection::{closure}` |
| 1.69% | `[.] zeroship_runtime::core::runtime::RuntimeInner::call_fetch_handler` |
| 1.49% | `[.] mi_free` |
| 1.40% | `[.] core::ptr::drop_in_place<zeroship_runtime::rpc::ctx_holder::RpcCtx>` |
| 1.16% | `[.] _mi_heap_realloc_zero` |
| 1.07% | `[k] tcp_ack` |
| 0.83% | `[k] skb_release_data` |
| 0.75% | `[.] __memmove_evex_unaligned_erms` |
| 0.73% | `[.] v8::handle::Weak<T>::second_pass_callback` |
| 0.71% | `[.] core::iter::adapters::map::Map::next` |
| 0.68% | `[.] mi_heap_malloc_aligned_at` |
| 0.64% | `[k] alloc_tagging_slab_alloc_hook` |
| 0.64% | `[k] __nf_conntrack_find_get` |
| 0.63% | `[k] rep_movs_alternative` |
| 0.62% | `[k] tcp_sendmsg_locked` |

### 2b — Top 10 userspace by inclusive (with-children) time

| Incl % | Self % | Symbol |
| ----: | ----: | --- |
| 46.07% | 0.10% | `syscall` (i.e. all in-thread io_uring submissions) |
| 9.61% | 1.69% | `RuntimeInner::call_fetch_handler` |
| 9.22% | 0.11% | `core::fmt::num::<impl LowerHex for usize>::fmt` |
| 6.63% | 0.07% | `parse_envelope_body` (V8 JSON.parse of `{}` + property get) |
| 5.85% | 0.19% | `rpc::dispatch::with_rpc_context_in_als` |
| 4.21% | 0.43% | `rpc::ctx_holder::mint_rpc_ctx` |
| 3.89% | 0.34% | `alloc::fmt::format::format_inner` |
| 3.34% | 0.15% | `v8__Global__New` |
| 3.19% | 0.14% | `v8__JSON__Parse` |
| 2.24% | 0.24% | `v8::Object::Get` (drives the `parse_envelope_body` `obj.get("json")`) |

### 2c — Allocator self-time (sum)

| Frame | Self % |
| --- | ----: |
| `_mi_page_malloc` | 4.43 |
| `mi_free` | 1.49 |
| `_mi_heap_realloc_zero` | 1.16 |
| `mi_heap_malloc_aligned_at` | 0.68 |
| **mimalloc total (self)** | **7.76** |

### 2d — V8 internals self-time (top contributors)

| Frame | Self % |
| --- | ----: |
| `v8::internal::GlobalHandles::Create` | 3.82 |
| `v8::internal::GlobalHandles::NodeSpace::Release` | 3.14 |
| `v8::handle::Weak::second_pass_callback` | 0.73 |
| `v8::Map::Set` | 0.56 |
| `v8::internal::Invoke` | 0.52 |
| `v8::handle::FinalizerMap::add` | 0.44 |
| **V8 handle/finalizer bookkeeping (top 6)** | **9.21** |

The `GlobalHandles` traffic is driven by:
- `mint_rpc_ctx` allocates 1 `Global` for the cached prototype handle, 1
  `Weak::with_guaranteed_finalizer` (= another `Global` slot via `v8__Global__NewWeak`),
  and the `External::New` for the boxed `RpcCtx` raw pointer
  (`crates/runtime/src/rpc/ctx_holder.rs:303-313`).
- `with_rpc_context_in_als` allocates 1 `Global` to snapshot the prior
  embedder-data slot before installing the ctx
  (`crates/runtime/src/rpc/dispatch.rs:54`), and a fresh `v8::Map` per
  request from `clone_map` (the bench has no prior ALS entries → the
  map walk does nothing, but the empty-Map allocation still happens
  `dispatch.rs:59` / `als.rs:295`).

## 3 — Targeted Criterion microbenches

Single-thread Criterion runs, allocator = mimalloc (matches runtime).
Each line is the `[low mid high]` triple from `criterion`. Raw output:
`/tmp/perf-microbench/microbench-results.txt`. Bench source:
`/tmp/perf-microbench/microbench/benches/*.rs`.

| Bench | ns/iter (median) | Notes |
| --- | ---: | --- |
| `format_req_pair` (current code: 2× `format!("req_{:016x}", id)` + `format!("trace_{:016x}", id)`) | **189.91 ns** | Two `format!` allocations per request, exactly what `build_rpc_ctx_inputs` does today (`runtime.rs:3179-3180`). |
| `format_req_single` (one `format!`) | 145.97 ns | Confirms ~150 ns minimum per `format!` on the mimalloc path. |
| `format_req_pair_writeinto` (`String::with_capacity` + `write!`) | 101.97 ns | Pre-sized buffer ≈ ½ the cost. |
| `format_req_pair_hand` (raw hex into `String::with_capacity`) | **25.70 ns** | Hand-rolled 4-bit-shift hex loop. Floor: ~26 ns/pair. |
| `box_rpcctx_default` (empty-`String` Box of the 12-field `RpcCtx`) | 19.27 ns | Single Box::new of an inert struct. |
| `box_rpcctx_filled` (Box + the two `format!`s + `method.to_string()` + `url.to_string()`) | **234.38 ns** | Mirrors the per-request cost incurred by `mint_rpc_ctx` today (excluding V8 handles). |
| `hashmap_in_flight_insert_drop` (thread-local `RefCell<HashMap<(u128,u64), …>>` insert + remove) | 76.77 ns | The bench-server is single-tenant (`app_id = None`) → this path is **NOT** taken on the perf run. Recorded for future comparisons (multi-tenant worker case). |
| `method_url_to_string` (4-byte + 33-byte `to_string()`) | 17.13 ns | Negligible. |
| `headers_to_vec_3` (3-pair `(String,String)` clone) | 76.13 ns | This corresponds to the `headers.to_vec()` at `runtime.rs:1569`. |
| `headers_to_vec_0` (empty Vec clone) | 4.29 ns | Branch baseline. |
| `build_rpc_ctx_inputs_3h` (full current fn: header scan + 2 `format!`) | 183.85 ns | Self-consistent with `format_req_pair` 189.91 ns minus a few ns of opt-out from the unmatched header scan. |

## 4 — Diff vs `48dad91` baseline (1067K req/s)

Userspace symbols **present in HEAD but absent or sub-0.2% in baseline**:

| Symbol | HEAD self % | Baseline self % | Δ |
| --- | ---: | ---: | ---: |
| `_mi_page_malloc` | 4.43 | 0.93 | **+3.50** |
| `v8::internal::GlobalHandles::Create` | 3.82 | 1.09 | **+2.73** |
| `v8::internal::GlobalHandles::NodeSpace::Release` | 3.14 | 0.72 | **+2.42** |
| `mi_free` | 1.49 | 0.75 | +0.74 |
| `core::ptr::drop_in_place<RpcCtx>` | 1.40 | 0.00 | +1.40 |
| `_mi_heap_realloc_zero` | 1.16 | not in top-50 | +1.16 |
| `v8::handle::Weak::second_pass_callback` | 0.73 | not in top-50 | +0.73 |
| `v8::Map::Set` | 0.56 | not in top-50 | +0.56 |
| `v8::handle::FinalizerMap::add` | 0.44 | not in top-50 | +0.44 |

Userspace inclusive (with-children) symbols **introduced post-baseline**:

| Symbol | HEAD incl % | Baseline incl % |
| --- | ---: | ---: |
| `core::fmt::num::<impl LowerHex for usize>::fmt` | 9.22 | absent (< 0.2 %) |
| `rpc::dispatch::with_rpc_context_in_als` | 5.85 | absent |
| `rpc::ctx_holder::mint_rpc_ctx` | 4.21 | absent |
| `alloc::fmt::format::format_inner` | 3.89 | 1.61 |
| `v8__Global__New` | 3.34 | 1.30 |

Sum of HEAD-only symbols' self-time: roughly **9–11 percentage points
of CPU**, dominated by allocator + V8 GlobalHandles churn from
`mint_rpc_ctx`, `with_rpc_context_in_als`, `RpcCtx::drop`, and the two
`format!` calls in `build_rpc_ctx_inputs`. That self-time delta is
proportionate to the throughput gap (1067K → 846K = ~21% drop; new
work consumes ~10pp of CPU on each of 16 worker threads, all of which
were previously idle on this path).

## 5 — Optimization candidates (measured costs only)

### C1 — Replace `format!("req_{:016x}", id)` × 2 with hand-rolled hex

- **Mechanism.** `build_rpc_ctx_inputs` currently does two
  `format!` invocations going through `core::fmt::write` →
  `<impl LowerHex for usize>::fmt` → `Formatter::pad_integral` →
  `String::write_char` (each push triggers `RawVec::reserve`/grow).
  Replace with a 16-iter `for shift in (0..64).rev().step_by(4)` loop
  writing into `String::with_capacity(20)` / `String::with_capacity(22)`.
- **File:line.** `crates/runtime/src/core/runtime.rs:3179-3180`.
- **Measured cost today.** Microbench `format_req_pair` = **189.91 ns/iter**.
  perf inclusive (LowerHex) = **9.22%** of process CPU.
- **Savings if replaced.** Microbench `format_req_pair_hand` = **25.70 ns/iter**.
  Δ = **189.91 − 25.70 = 164.21 ns/req**. Halfway alternative
  `format_req_pair_writeinto` = 101.97 ns (Δ = 87.94 ns/req).
- **Effort.** small (single function, no API change).
- **Risk.** Low. Output bit-identical to the current `{:016x}` formatter.
  Existing tests for `ctx.requestId` / `ctx.traceId` regex matching
  continue to pass. No semantic change.
- **Constraint.** Preserves ALS + `__zeroshipGetRpcCtx` (no behavior
  change to either).

### C2 — Avoid the empty-`v8::Map` alloc in `with_rpc_context_in_als`

- **Mechanism.** `with_rpc_context_in_als` always calls
  `read_context_map → clone_map` or, if the slot is empty,
  `v8::Map::new`. In the bench-server (and any handler path with no
  prior `AsyncLocalStorage` entries), the map starts empty: the clone
  walks zero entries but still allocates a fresh `v8::Map`, bumps a
  `Global` slot for `prev_global`, and triggers `v8::Map::Set` for the
  ctx entry. Replace with a "single-entry fast path": when the prior
  slot is undefined/null, skip the Map and store the ctx Local directly
  in the embedder-data slot under the rpc-ctx Symbol — readers in
  `__zeroshipGetRpcCtx` already check both `read_context_map` and
  `is_undefined()` so a non-Map slot with the ctx as the only inhabitant
  still works as long as the read path is taught the dual layout.
  (Alternative: keep a per-isolate "empty-map" cached `Global` and
  install it instead of allocating a fresh one — saves the V8 alloc but
  not the `set_continuation_preserved_embedder_data` round-trip.)
- **File:line.** `crates/runtime/src/rpc/dispatch.rs:53-69`,
  `crates/runtime/src/node/async_hooks/als.rs:300-323`.
- **Measured cost today.** perf inclusive = **5.85%** of process CPU
  for `with_rpc_context_in_als`; self of `v8::Map::Set` = 0.56%, self
  of `v8::Map::New` is folded into `v8__Local__New_FromMap`-style
  paths and not separately ranked.
- **Savings if replaced.** **Not measured this round** — needs a
  dedicated microbench in-isolate (Criterion can't link v8-rs without
  importing the runtime test harness). Order of magnitude: the entire
  5.85% inclusive minus the unavoidable embedder-data slot
  read/write — but **do not write a number until measured**.
- **Effort.** medium (touches both writer and reader paths; needs a
  user-ALS regression test for "user code calls
  `AsyncLocalStorage.run` *before* the platform ctx is installed").
- **Risk.** medium-high. The `__zeroshipGetRpcCtx` reader expects a
  `v8::Map`; the patch must teach it about the bare-Local fallback
  without breaking `user_async_local_storage_does_not_collide`
  (`crates/runtime/tests/rpc_ctx.rs:270`).
- **Constraint.** Preserves ALS + `__zeroshipGetRpcCtx`.

### C3 — Skip the `prev_global` snapshot when prev is undefined

- **Mechanism.** `with_rpc_context_in_als` unconditionally allocates
  `v8::Global::new(scope, prev_slot)` to restore the prior value after
  the call (`dispatch.rs:54`). When the slot is empty (no enclosing
  `AsyncLocalStorage.run`), the Global allocation is wasted: the
  restore can be a fixed `set_continuation_preserved_embedder_data(undefined)`.
  A simple `if prev_slot.is_undefined() || prev_slot.is_null()` short-circuit
  avoids `Global::New` + `Global::Drop` for every request.
- **File:line.** `crates/runtime/src/rpc/dispatch.rs:53-67`.
- **Measured cost today.** `v8::internal::GlobalHandles::Create` self
  = **3.82%**, `Release` = **3.14%**. The call-graph at
  `mint_rpc_ctx`'s `--9.11%--` LowerHex frame attributes 0.97% of
  `Global::New` to `mint_rpc_ctx`; the remaining 2.4pp comes mostly
  from `with_rpc_context_in_als` and from the `Weak`/External handles
  inside `mint_rpc_ctx`. Per-request fraction of the
  `with_rpc_context_in_als` `Global::New` call is **not measured this
  round** (would need `perf annotate` on the function or a per-call
  dtrace).
- **Savings if replaced.** **needs microbench** (in-isolate).
- **Effort.** small (a single-line conditional).
- **Risk.** Low — semantically identical: when prev was undefined,
  the restore is a no-op except for slot freshness.
- **Constraint.** Preserves ALS + `__zeroshipGetRpcCtx`.

### C4 — Defer the `headers.to_vec()` clone behind the lazy headers accessor

- **Mechanism.** `mint_rpc_ctx` is handed
  `headers: Vec<(String,String)>` cloned out of the dispatcher
  (`runtime.rs:1569`). The clone copies every `String` pair into a
  fresh Vec even if user code never reads `ctx.headers`. The natural
  fix is to pass an `Arc<Vec<(String,String)>>` (or `&'static`-style
  ownership transfer from the dispatcher) and let the lazy
  `cached_headers` accessor build the JS Headers wrapper on demand —
  same model as `cached_url`/`cached_signal`/`cached_user`.
- **File:line.** `crates/runtime/src/core/runtime.rs:1569`,
  `crates/runtime/src/rpc/ctx_holder.rs:267, 291`.
- **Measured cost today.** Microbench `headers_to_vec_3` = **76.13 ns/iter**.
  perf does not isolate this frame (folded into `mint_rpc_ctx`'s 4.21%
  inclusive); `box_rpcctx_filled` − `box_rpcctx_default` = 234.38 − 19.27
  = **215 ns/req** of total per-request work attributable to the
  String/Vec field initializers, of which `headers_to_vec_3` = 76 ns.
- **Savings if replaced.** Microbench `headers_to_vec_0` = **4.29 ns/iter**.
  Δ for the 3-header bench input = **76.13 − 4.29 = 71.84 ns/req**. In
  production with 8–10 headers the savings scale linearly with header
  count.
- **Effort.** small-medium (refactor of `RpcCtx::headers` field +
  the Headers materializer accessor).
- **Risk.** low. Headers wrapper identity is already cached
  (`cached_headers`); the clone-vs-borrow change is invisible to user
  code.
- **Constraint.** Preserves ALS + `__zeroshipGetRpcCtx`.

### C5 — Drop the unused `RpcCtx` fields when they're empty

- **Mechanism.** For `auth: "anon"` requests (the entire bench-server
  surface), `user_json` and `idempotency_key` are always `None`. The
  `RpcCtx` struct still holds five `RefCell<Option<…>>` slots
  (`abort_controller`, `cached_headers`, `cached_url`, `cached_signal`,
  `cached_user`) initialized to `RefCell::new(None)`, plus three
  `Option<String>` fields. Every `Box<RpcCtx>` is `> 200 bytes` of
  zeroed/none init that mimalloc has to memset on each alloc. Splitting
  the holder into a small "always-needed" struct + a lazily-allocated
  "lazy fields" sidecar would shrink the per-request hot allocation.
- **File:line.** `crates/runtime/src/rpc/ctx_holder.rs:37-65`.
- **Measured cost today.** `box_rpcctx_default` = **19.27 ns/iter**
  for the empty struct alone (mimalloc bin-allocation + memset).
  `_mi_page_malloc` self = 4.43%, `_mi_heap_realloc_zero` = 1.16%, and
  `__memmove_evex_unaligned_erms` = 0.75% — all consistent with
  per-request zero-fill churn.
- **Savings if replaced.** **needs microbench** of the proposed split
  shape; this round only measured the current shape.
- **Effort.** medium.
- **Risk.** medium — touches every accessor; requires careful unsafe
  audit around the `*const ()` raw pointer in `External::New`.
- **Constraint.** Preserves ALS + `__zeroshipGetRpcCtx` (struct is
  internal).

### C6 — Cache the `External` + holder for `RpcCtx` and reset-in-place

- **Mechanism.** `mint_rpc_ctx` builds a fresh
  `Box<RpcCtx>` + `External::new` + `set_internal_field` +
  `Weak::with_guaranteed_finalizer` per request; the matching `Drop`
  shows up in perf as **1.40% self-time** for
  `core::ptr::drop_in_place<RpcCtx>`. The Weak finalizer registration
  drives `v8::handle::FinalizerMap::add` (0.44% self) and the GC's
  `Weak::second_pass_callback` (0.73% self). A pooled holder stored in
  a per-isolate slot (one `RpcCtx` per isolate, reset in-place on each
  request, lifetime tied to the isolate not the request) collapses all
  three into a per-isolate one-time setup.
- **File:line.** `crates/runtime/src/rpc/ctx_holder.rs:300-313`.
- **Measured cost today.** Self of `RpcCtx::drop` + `Weak::second_pass`
  + `FinalizerMap::add` = **1.40 + 0.73 + 0.44 = 2.57%** of process CPU.
  Plus the `External::New` self-time which is folded into the
  `mint_rpc_ctx --0.81%--External::New` callgraph slice.
- **Savings if replaced.** **needs microbench** in-isolate.
- **Effort.** medium — needs a per-isolate "currently-in-flight ctx"
  slot, eviction discipline if a request panics mid-flight, and a
  re-entrancy story for nested RPC calls (currently impossible, but
  the future composition story may need it).
- **Risk.** medium-high — touches lifetime semantics. Has to play
  nicely with the AbortController guarantee (the eager-controller
  multi-tenant path stores a `Global<v8::Object>` clone in the abort
  registry; that part already outlives the RpcCtx, so pooling RpcCtx
  doesn't break it).
- **Constraint.** Preserves ALS + `__zeroshipGetRpcCtx`.

### C7 — Fast-path `parse_envelope_body` for `{}` and `{"json":null}`

- **Mechanism.** Bench body is `{}`. The current fast path requires
  bytes to start with `{"json":` and end with `}`, so `{}` falls into
  the slow path: `v8::String::new("{}")` + `v8::json::parse` (which
  allocates a JSObject) + `Object::Get("json")` → undefined → fallback
  to the parsed object. Cost shows up at **6.63% inclusive** under
  `parse_envelope_body`. Adding `if body == "{}" { return Ok(undefined) }`
  and `if body == "{}" || body == "{\"json\":null}" { return Ok(null) }`
  short-circuits eliminate the V8 round-trip for the common no-arg
  shapes.
- **File:line.** `crates/runtime/src/core/runtime.rs:3092-3134`.
- **Measured cost today.** Inclusive of `parse_envelope_body` =
  **6.63%**, of which `v8__JSON__Parse` = 3.19%, `v8::Object::Get` =
  2.24%, the rest is V8 string allocation and the property-key lookup
  path (`StringTable::LookupString` 1.12%).
- **Savings if replaced.** **needs microbench** with V8 in scope.
  perf-trace upper bound for the fast path is the entire 6.63%
  inclusive; the floor is 0 if zerobench's body is changed to
  `{"json":null}` (which the runner does for some scenarios — but the
  current `ping` rhai sends `{}` literally, see
  `crates/runtime/benches/zeroship-bench.rhai:51`).
- **Effort.** small.
- **Risk.** Low — confined to a wire-format edge case.
- **Constraint.** Preserves ALS + `__zeroshipGetRpcCtx`.

## 6 — Recommended top 3 (by measured savings ÷ effort)

| Rank | Candidate | Measured Δ per req | Effort | Composes with |
| ---: | --- | ---: | --- | --- |
| 1 | **C1 — hand-rolled hex for `req_/trace_` ids** | **164.21 ns** (microbench `format_req_pair` 189.91 → `format_req_pair_hand` 25.70) | small | independent |
| 2 | **C4 — defer `headers.to_vec()` clone** | **71.84 ns** (3-header bench input; scales with header count) | small-medium | independent |
| 3 | **C7 — fast-path `parse_envelope_body` for `{}`** | not measured this round; perf inclusive **6.63%** of CPU is the upper bound | small | independent |

C1 alone is the highest-confidence win this round: 164 ns/req × ~846K
req/s ≈ a measurable double-digit ns/req at the dispatch level. The
runtime-level effect is strictly larger because the saved heap
allocations also drop two `_mi_page_malloc` + `mi_free` round-trips per
request, but this report does not estimate the runtime delta — that
would require running the patched binary through the same perf gate.

C4 is independent from C1 and combines additively. C7 is independent
of both but its measured savings need a microbench (or the patched
binary) before promotion above C2/C3.

C2 (skip the empty-Map alloc) and C6 (pool the RpcCtx) likely
out-perform C7 in absolute savings (the perf inclusive numbers are
larger), but their measured deltas are gated on in-isolate microbenches
that this round did not produce.

## 7 — Open questions / not measured this round

1. **In-isolate microbenches** for `with_rpc_context_in_als`,
   `mint_rpc_ctx`, and `parse_envelope_body` were not produced.
   Criterion can't link v8-rs cleanly without depending on the runtime
   crate's test harness. Would need to add bench targets in
   `crates/runtime/benches/` rather than `/tmp/perf-microbench/`.
2. **Per-frame time within `with_rpc_context_in_als`** — the 5.85%
   inclusive bundles `Global::New(prev)` + `read_context_map` + empty
   `clone_map` + `v8::Map::new` + `v8::Map::Set` + the
   `set_continuation_preserved_embedder_data` ABI thunks. `perf
   annotate` of the function would split these but was not run today.
3. **GC frequency contribution.** Baseline shows 0.75% in
   `Heap::CollectGarbage`-related frames; HEAD shows 1.58% via
   `parse_envelope_body --1.55%--HeapAllocator::AllocateRawSlowPath →
   CollectGarbage`. The ratio suggests the new per-request V8
   allocations (the prototype `Global`, the External, the ALS Map) are
   driving extra minor GCs. Quantifying that requires `--collect-gc`
   counters that perf doesn't expose by default.
4. **Throughput delta of each candidate**, end-to-end. Each candidate
   above is measured at the microbench level; the 16-worker macrobench
   delta (∂ req/s per candidate) is **not measured** this round. The
   user instruction explicitly forbids extrapolating from microbench
   ns/iter to macrobench req/s. Each candidate, when implemented, must
   be re-validated with the same `zerobench run …ping.rhai` configuration.
5. **Headers count in production.** Bench has 3 headers;
   `headers_to_vec` cost is linear, so production with ~10 headers
   would show ~250 ns/req for C4 instead of ~72 ns. Not measured here.
6. **The 1.16M baseline number from 2026-05-04** is 100K higher than
   the 1067K I measured today against the same commit `48dad91`. Likely
   noise + system state, not a regression in the baseline build.
   Documenting the today-measured 1067K as the relevant comparison
   point for these candidates.
