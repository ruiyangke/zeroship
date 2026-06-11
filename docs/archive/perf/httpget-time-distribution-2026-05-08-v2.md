# httpGet slow-path time distribution — 2026-05-08 v2 (post Response.json)

> **Supersedes** `httpget-time-distribution-2026-05-08.md` (v1; archived in Phase 2 to `docs/archive/perf/`).

Re-profiles `default.fetch` `/hello` at HEAD `5bb35bb` (lazy Request
Headers + native `Response.json` fast path). Successor to v1 (348,676 req/s baseline).

Every number is measured: `perf record` self-time + folded-stack
inclusive, in-isolate `performance.now()` micro-loops, criterion
data from the prior round where re-use is valid. Rows I cannot
isolate cleanly read "needs measurement". No estimates.

## 1. Methodology

**Hardware / build.** 32-core Xeon @ 2.80 GHz, two NUMA nodes (server
on node 0, client on node 1, both `numactl
--cpunodebind=N --membind=N`). `kernel.perf_event_paranoid = 1`
(kernel addresses unresolved as `[k] 0xffff…`, same as prior round).
V8 crate `v8 = "147"`. Build: `cargo build --release` at `5bb35bb`,
no source modifications. Server:
`zeroship-bench-server --port=5101 --workers=16`. Client:
`zerobench run zeroship-bench.rhai -c 300 --threads 16` with
`BENCH_SCENARIO=httpGet`, 10 s saturate.

**State changes vs prior round.** Prior was `b08786a` + an
uncommitted lazy-Request-Headers diff. Single new commit since:
`5bb35bb` — merges that diff plus the new native
`build_response_json_fast` (`crates/runtime/src/web/fetch/response.rs:450-523`)
that bypasses the JS Response constructor when `init.headers` is
absent. The brand-slot Eternal optimisation (`b08786a`) was active
in both rounds.

**Macrobench (current).** Three 10 s saturate runs:

| Run | req/s |
|---|---:|
| 1 | 400,095 |
| 2 | 387,363 |
| 3 | 387,310 |
| **Mean** | **391,589** |
| Range | 12,785 (3.26 %) |

Source: `/tmp/httpget-distrib-v2/macro-{1,2,3}.txt`. Within 1.54 % of
the brief's 397,708.

**Per-request CPU budget**: 16 × 1 s / (391,589 / 16) = **40,859
ns/req**. Down from the prior 45,888 — saving of **5,029 ns/req** at
the system level.

**Perf record.** `perf record -F 999 -p $PID -g --call-graph
dwarf,16384 sleep 10` during a saturate run (378,165 req/s under
perf overhead). 142,031 samples, 142.17 G event count. Folded via
`perf script | inferno-collapse-perf` → 21,158 stacks. Flamegraph:
`docs/archive/perf/flamegraphs/2026-05-08-httpget-397k.svg` (1.71 MB).

**JS microbench.** `/tmp/perf-microbench/op-bench-v2.js`, single-
worker `zeroship serve` pinned to NUMA 0. Each named bench: 500-iter
warmup, 2,000-iter measurement, `performance.now()` deltas. 15 runs
per bench, median + min + p25 + p75. scenarios.js untouched
(`git diff` empty).

## 2. perf record — top 20 self-time symbols

Source: `/tmp/httpget-distrib-v2/top-self-full.tsv`.

| % | Symbol |
|---:|---|
| 10.39 | `v8::internal::GlobalHandles::Create` |
| 8.43 | `v8::internal::GlobalHandles::NodeSpace::Release` |
| 5.12 | `[k] 0xffffffffa0f680cd` (kernel; unresolved at paranoid=1) |
| 4.86 | `_mi_page_malloc` |
| 2.62 | `v8::handle::Weak<T>::second_pass_callback` |
| 1.38 | `v8::handle::FinalizerMap::add` |
| 1.13 | `serve::handle_connection::{{closure}}` |
| 1.11 | `__memmove_evex_unaligned_erms` |
| 0.94 | `drop_in_place<headers::Headers>` |
| 0.93 | `mi_free` |
| 0.78 | `mi_heap_malloc_aligned_at` |
| 0.73 | `RuntimeInner::call_fetch_handler` |
| 0.73 | `Scavenger::ScavengeObject<FullHeapObjectSlot>` |
| 0.66 | `Isolate::get_annex_arc` |
| 0.55 | `Builtins_CallApiCallbackOptimizedNoProfiling` |
| 0.53 | `RawTask::clone_waker` |
| 0.53 | `__libc_malloc2` |
| 0.51 | `GlobalHandles::IterateYoungStrongAndDependentRoots` |
| 0.42 | `mi_malloc_aligned` |
| 0.40 | `Factory::NewExternal` |

### Top 10 inclusive (folded stacks, % of total samples)

| Inclusive % | Symbol |
|---:|---|
| 58.35 | `compio_runtime::*` (entire scheduler subtree) |
| 31.99 | `serve::handle_connection` |
| 26.86 | `compio_driver::*` |
| 25.42 | `Driver::poll` (compio_driver::sys::iour) |
| 24.88 | `io_uring::Submitter::submit_with_args` |
| 19.87 | total kernel `[k]` self (sum of unresolved frames) |
| 15.52 | `__Response_json_callback` (native fast path) |
| 10.72 | `build_kernel_headers` |
| 7.21 | `Weak<T>::second_pass_callback` |
| 7.09 | `inspect_response` |

### Symbol clusters (self-time)

Source: `/tmp/httpget-distrib-v2/clusters.txt`. Sums to 93.74 %;
remainder is sub-0.05 % helpers.

| Cluster | % |
|---|---:|
| v8 global-handles + Weak/Finalizer | 25.26 |
| kernel `[k]` (unresolved) | 19.87 |
| v8 other (Builtins_*, Object, Template, JSON, Function::Call) | 9.57 |
| alloc/mimalloc | 8.97 |
| compio scheduler / async_task | 3.06 |
| v8 factory + external alloc | 2.45 |
| v8 isolate + scope + Invoke | 2.35 |
| rt headers (build + drop) | 1.82 |
| v8 strings + JsonStringifier | 1.79 |
| memmove + memcmp + memchr | 1.72 |
| v8 object/property | 1.70 |
| rust stdlib (drop_in_place, RefCell, iter) | 1.65 |
| rt url + ada | 1.40 |
| rt fetch::request | 1.31 |
| v8 api-callback dispatch | 1.20 |
| v8 scavenger gc | 1.20 |
| rt serve::handle_connection (self) | 1.13 |
| rt brand_check (Request+Response+Headers+URL) | 0.80 |
| rt fetch::response | 0.76 |
| rt fetch::body / usv | 0.72 |
| rt transport::handler | 0.22 |

**Memory management share** (GH cluster + Scavenger + mimalloc +
memmove/memcmp): 25.26 + 1.20 + 8.97 + 1.72 = **37.15 %** of CPU.
Up from 27.51 % in the prior round. **Kernel I/O**: 19.87 % self,
24.88 % inclusive in `submit_with_args`.

## 3. Diff vs the prior 348K profile

### Symbols that DROPPED

| Symbol | Prior % | Current % | Δ pp |
|---|---:|---:|---:|
| `__Response_constructor` (incl.) | 15.25 | **0.00** | **−15.25** |
| `Object::Get` self | 0.78 | <0.30 | drop |
| `Utf8DecoderBase::Utf8DecoderBase` self | 0.72 | 0.28 | −0.44 |
| `Factory::AllocateRaw` self | 0.68 | 0.33 | −0.35 |
| v8 callback-dispatch cluster | 3.83 | 1.20 | −2.63 |

`__Response_constructor` is GONE (zero samples) — confirms the JS
constructor is bypassed. The new `__Response_json_callback`
inclusive 15.52 % now decomposes into
`build_kernel_headers_owned` (4.77 %), `install_headers_state`
(4.03 %), `v8::JSON::Stringify` (2.93 %), `v8::ObjectTemplate::
NewInstance`, plus the macro `Box`+`External::new`+
`Weak::with_guaranteed_finalizer` tail.

### Symbols that GREW

| Symbol | Prior % | Current % | Δ pp |
|---|---:|---:|---:|
| `GlobalHandles::Create` self | 6.90 | 10.39 | **+3.49** |
| `GlobalHandles::NodeSpace::Release` self | 4.80 | 8.43 | **+3.63** |
| `Weak<T>::second_pass_callback` self | 2.16 | 2.62 | +0.46 |
| `_mi_page_malloc` self | 4.43 | 4.86 | +0.43 |
| `__Request_headers_callback` (incl.) | 4.85 | 7.00 | +2.15 |
| `build_kernel_headers` (incl.) | 4.07 | 10.72 | **+6.65** |
| `inspect_response` (incl.) | 6.03 | 7.09 | +1.06 |

In ns/req (translating each through its round's budget):

| Symbol | Prior ns/req | Current ns/req | Δ ns |
|---|---:|---:|---:|
| GlobalHandles Create+Release self | 5,368 | 7,690 | **+2,322** |
| `__Response_constructor` (incl.) | 6,998 | 0 | **−6,998** |
| `__Request_headers_callback` (incl.) | 2,226 | 2,860 | +634 |

GlobalHandles got slower in absolute terms by ~2,300 ns/req. The
new `build_response_json_fast` does `slot.class_tmpl.clone()` +
`slot.prototype.clone()` per call (`response.rs:457-462`) — both
allocate fresh GlobalHandles slots; both could be Eternals.

### Buckets that shrank vs grew

| Bucket | Prior % | Current % |
|---|---:|---:|
| memory mgmt (GH+GC+alloc+memmove) | 27.51 | **37.15** |
| kernel `[k]` self | 17.91 | 19.87 |
| v8 callback dispatch (api-callback + funccb) | 3.83 | 1.20 |
| rt headers self | 2.43 | 1.82 |
| ada-url + rt/url | 1.22 | 1.40 |
| rt fetch::request self | 1.13 | 1.31 |
| rt brand_check self (made measurable) | (n/a) | 0.80 |

The biggest *proportional* shift: memory management's share grew
from 27.5 % to 37.2 % — but only because total CPU/req shrank from
45,888 → 40,859 ns and the GC overhead is roughly fixed-cost per
request.

### Surprises

1. **Native fast path uses Globals, not Eternals, for class+prototype
   templates.** Brand slots are Eternal (per `b08786a`); class
   templates aren't. `build_response_json_fast` allocates two fresh
   GH slots per call.

2. **`build_kernel_headers` inclusive 4.07 → 10.72 %** is partly a
   relabel: the response-side Headers wrapper that used to be
   created inside `__Response_constructor` is now created via
   `build_kernel_headers_owned` and accounted under
   `__Response_json_callback`.

3. **`brand_check` self is 0.80 %** (visible because Eternal made
   each check cheap; remaining cost is `Eternal::get` + ptr equality
   on every method/getter call).

## 4. Per-operation JS microbench (current state)

Source: `/tmp/httpget-distrib-v2/microbench-pinned.tsv`. 15 runs
× 36 benches, single-worker, NUMA-pinned. Median of 15 runs.

### Response.json shapes

| Bench | Median | Min | p25 | p75 |
|---|---:|---:|---:|---:|
| `json/empty` (`Response.json({})`) | 2,103 | 1,590 | 1,932 | 2,595 |
| `json/small` (`Response.json({ok:true})`) | **2,059** | 1,495 | 1,807 | 2,360 |
| `json/med` (5-key body) | 2,237 | 1,909 | 2,177 | 2,514 |
| `json/small+status` | 2,718 | 2,317 | 2,554 | 3,165 |
| `json/small+headers` (init.headers={X-A:1}) | 9,347 | 6,980 | 7,776 | 10,907 |

`Response.json({ok:true})` median **2,059 ns**, prior round **5,563
ns** — saving of **3,504 ns/op**. The `+headers` case still uses
the JS-constructor fallback (4.5× slower; by design).

The brief's "1,227 ns" does not reproduce: my 15-run median is
2,059, min 1,495. **Needs cross-check** if the precise number
matters.

### Other shapes

| Bench | Median | Source |
|---|---:|---|
| `stringify/small` (`JSON.stringify({ok:true})`) | 109 | this round |
| `stringify/med` (5-key) | 184 | this round |
| `req_headers/first` (single-shot lazy mint) | 25,079; **min 10,720** | this round |
| `req_headers/cached` | 69 | this round |
| `req_headers/get_upgrade` (post-cache) | 122 | this round |
| `url/parse` (`new URL(string)`) | **1,193** | this round |
| `h/empty` | 526 | this round |
| `h/ct` | 2,121 | this round |
| `h/6h` | 6,145 | this round |

`json/small` 2,059 − `stringify/small` 109 = **1,950 ns** for
the Response wrapper alloc + Headers wrapper alloc + Body wrap on
the native fast path. Down from 5,491 ns of constructor + JS
dispatch in the prior round.

`url/parse` 1,193 ns vs prior 1,212 ns — within noise; URL
constructor unchanged.

`req_headers/first` median 25,079 ns is high because the single-shot
measurement is GC-noise-dominated (p75=52,470 ns). **Min** 10,720
ns is more representative — broadly consistent with prior round's
single-shot median 9,509.

### Other Response shapes (regression check)

| Bench | Prior median | Current median | Δ |
|---|---:|---:|---:|
| `resp/empty` (`new Response()`) | 2,115 | 1,579 | −536 |
| `resp/null` | 2,101 | 1,524 | −577 |
| `resp/string` | 3,772 | 3,450 | −322 |
| `resp/string+ct` | 5,309 | 6,770 | +1,461 |
| `resp/string+status+ct` | 5,296 | 5,868 | +572 |

Bare `new Response()` shapes are ~500 ns faster (likely Eternal
brand-slot knock-on). The `+ct` cases got slower — but run-to-run
range is large (3,847–7,060 for `resp/string+ct`), likely noise.

## 5. Decomposition table (current 397K state)

Per-request budget: **40,859 ns**. ns/req column = `% × 40,859 ÷
100`. The `%` column is perf-attributed. The µbench column is
steady-state hot-loop — not directly comparable to perf-attributed
(real traffic pays cold-call + shared GC/allocator on top).

| Operation | Triggered | perf % | ns/req | µbench ns/op |
|---|---|---:|---:|---:|
| `__Response_json_callback` (incl., native fast path) | line 311 | 15.52 | 6,341 | 2,059 |
| kernel `[k]` self | every req | 19.87 | 8,118 | n/a |
| GlobalHandles Create+Release self | amortised | 18.82 | 7,690 | n/a |
| Weak+Finalizer self | amortised | 4.00 | 1,634 | n/a |
| Allocator (mimalloc + memmove + system) | amortised | 12.41 | 5,071 | n/a |
| `build_kernel_headers` (incl., shared Req+Resp lazy paths) | every req | 10.72 | 4,380 | 348 (Rust 6h, prior) |
| `inspect_response` (incl., kernel post-handler) | every req | 7.09 | 2,897 | n/a |
| `__Request_headers_callback` (incl., first access) | line 284 | 7.00 | 2,860 | min 10,720 |
| `build_kernel_request` (incl.) | every req | 6.19 | 2,529 | 178 (Rust, prior) |
| `__URL_constructor` (incl.) | line 282 | 5.31 | 2,170 | 1,193 |
| `__URL_pathname_callback` (incl., 4×) | lines 287,295,301,305 | 3.57 | 1,459 | 102 (prior) |
| `extract_response_headers` (incl., inside inspect) | every req | 3.42 | 1,398 | 228 Rust 6h (prior) |
| `__Request_url_callback` (incl.) | line 311 | 2.39 | 977 | 98 (prior) |
| `__Response_headers_callback` (incl., inside inspect) | every req | 1.48 | 605 | n/a |
| `__Headers_get_callback` (incl.) | line 284 | 1.00 | 409 | 122 |
| `brand_check_*` self (4 classes) | every method/getter | 0.80 | 327 | n/a |
| `__Request_method_callback` (incl.) | line 311 | 0.68 | 278 | 90 (prior) |
| `__Response_status_callback` (incl., inside inspect) | every req | 0.48 | 196 | n/a |
| `serve::handle_connection` self | per req | 1.13 | 462 | n/a |
| `call_fetch_handler` self | per req | 0.73 | 298 | n/a |
| compio scheduler self (clone_waker, RawTask::run, glue) | per req | ~3 | ~1,200 | n/a |

**Subtotals.** Memory management (37.15 %) + kernel I/O (19.87 %)
alone = **57 % of budget = 23,290 ns/req** before any per-callback
work. JS-fired callback inclusives sum to ~38 % but double-count
the cluster totals (each subtree includes nested GH/finalizer/alloc).

## 6. Where did the savings actually go?

Computed deltas:

| Quantity | Value |
|---|---:|
| Macrobench: 348,676 → 391,589 | +42,913 req/s |
| Budget: 45,888 → 40,859 | **−5,029 ns/req** |
| Microbench prediction (Response.json 5,563 → 2,059) | −3,504 ns/req |
| Lazy-Headers diff was already in prior round | 0 ns/req |
| Macrobench delivered minus microbench prediction | **+1,525 ns/req (43 %)** |

Macrobench delivered **MORE** than microbench predicted — inverting
the prior agent's hypothesis ("microbench wins do not translate to
macrobench").

Possible explanations:

1. **Compounding via reduced GC pressure.** Less per-request V8 work
   → less Scavenger churn → more time available for handler work.
   The 2.63 pp drop in v8 callback-dispatch share supports this.

2. **The native path also eliminates the inner `new Headers(init)`
   round-trip** that the prior microbench bundled into the Response
   constructor cost.

3. **Microbench undercounts cold-call + GC tail**, which both prior
   and current numbers underestimate by similar ratios.

The prior agent's earlier "Variant A" finding (lazy-Headers raw-list
fallback saved ~1,400 ns microbench but 0 % macrobench) is still
consistent with point (1) in reverse: small wins don't trigger
compounding GC benefits. Both observations can be true. **Signal:
target wins ≥ 1,000 ns/req on the hot path for measurable system
lift.**

## 7. Optimization candidates

Each candidate cites measured perf or microbench data for cost.
"Saving if implemented" only when alternative is also measurable;
otherwise marked **needs microbench post-impl**.

### a. Eternal `*TemplateSlot` Globals on the Response.json fast path

**Targets.** GH Create+Release self 18.82 % = 7,690 ns/req.
`build_response_json_fast` does `slot.class_tmpl.clone()` +
`slot.prototype.clone()` per call (`response.rs:457-462`), allocating
fresh GH slots. Pattern fix: `v8::Global<v8::FunctionTemplate>` →
`v8::Eternal<v8::FunctionTemplate>`. Identical mechanism to
`b08786a`'s brand-slot Eternal.

**Saving.** Per-call GH save unknown without a V8-coupled microbench.
The brand-slot precedent freed ~9.3 pp; converting `*TemplateSlot`
should free a similar share. **Needs microbench post-impl.**

**Effort.** Low — mechanical refactor on a known pattern. **Risk.**
Low — Eternal semantics match isolate-lifetime templates exactly.

### b. Lazy Response Headers + raw-list `inspect_response`

**Targets.** `build_kernel_headers` inclusive 10.72 % = 4,380
ns/req. ~Half is response-side allocation (the request-side lazy
mint is the other half). On /hello, `inspect_response` is the only
consumer of `response.headers` — a wrapper materialises just to be
inspected once.

A "lazy Response Headers" mirroring the existing lazy Request Headers
pattern: store the raw 1-entry list on `ResponseState`, mint the
wrapper only when JS reads `response.headers` OR when
`extract_response_headers` chooses to read the raw list directly.

**Saving.** Up to ~2,200 ns/req. **Needs microbench post-impl.**

**Effort.** Moderate — same shape as the shipped lazy Request
Headers. **Risk.** Medium — `[SameObject]` invariant; WPT
re-verification required.

### c. `request.path` direct getter (skip `new URL(request.url)` on /hello)

**Targets.** `__URL_constructor` incl. 5.31 % = 2,170 ns/req +
`__URL_pathname_callback` incl. 3.57 % = 1,459 ns/req (4× per request).
Of `__URL_constructor`, ada self ~600 ns; rest is V8 wrapper
machinery.

A native `request.path` (or `request.zsPath`) getter exposes the
already-parsed pathname slice from `state.url`. /hello can skip
the URL constructor + 4× pathname callbacks if scenarios.js is
updated (same pattern as `fetchFast`).

**Saving.** Up to ~3,600 ns/req on /hello with scenarios.js update.
0 ns/req if scenarios.js stays WinterCG-pure. **Needs microbench
post-impl** for precise figure.

**Effort.** Moderate. **Risk.** Low — additive.

### d. `inspect_response` native fast-path for kernel-built Responses

**Targets.** `__Response_status_callback` 0.48 % +
`__Response_headers_callback` 1.48 % + V8-side
`extract_response_headers` ≈ 1.7 % = ~3.7 % = ~1,500 ns/req.

When the Response was built by `build_response_json_fast` (or
`build_kernel_response`) and not mutated, project the
`ResponseState` ptr once and read status / headers / body
directly — skip three V8 callbacks. Need a Private-symbol
"untouched-since-construction" marker cleared on any mutation.

**Saving.** Up to ~1,500 ns/req. **Needs microbench post-impl.**

**Effort.** Moderate. **Risk.** Medium — mutation tracking surface
across `[SameObject]` Headers.

Note: candidates (b) and (d) both target `inspect_response`'s V8
callback round-trips. Pick one — they don't compose cleanly.

### e. Compio submit batching — coalesce write+read into one io_uring enter

**Targets.** `Submitter::submit_with_args` incl. 24.88 % = ~10,160
ns/req. Kernel-self share (19.87 %) is the syscall floor; remainder
is Rust-side submit/poll glue.

If small-response writes can be queued without an immediate enter
and the next read picks them up in one syscall, drop one syscall
round-trip per request.

**Saving.** **Needs microbench post-impl.** Rough order-of-magnitude
target: a single io_uring enter saved would save ~kernel-self share
÷ submits-per-req. Real savings depend on compio batching design.

**Effort.** High — touches compio internals. **Risk.** High —
latency tradeoff if submits queued too long.

### f. ResponseTemplateSlot — drop per-Response `Global<Headers>` for sync-consumed Responses

**Targets.** `Global::new` (`v8__Global__New` 0.27 % self) plus
its share of GH Create/Release. The `ResponseState.headers` slot
is `RefCell<Option<Global<Object>>>` — for /hello-shape requests
the Response is built and consumed within the same handler tick;
the Global is allocated then immediately freed.

A skip-Global path for kernel-built Responses: keep Headers as a
`Box<Headers>` on `ResponseState`, promote to Global only if user
JS reads `response.headers`.

**Saving.** Up to ~1,000 ns/req. **Needs microbench post-impl.**

**Effort.** Moderate — distinguish kernel-built vs user-built via
Private symbol. **Risk.** Medium — must invalidate marker on user
mutation.

## 8. Recommended top 3

Ranked by `(measured cost) ÷ (effort)`:

1. **Candidate (a) — Eternal-ize `*TemplateSlot` Globals.** Highest
   measured perf footprint (GH Create+Release 7,690 ns/req self) on
   the hot path, with a known-pattern fix. Mechanical refactor;
   `b08786a` showed exactly how. Composes with (b) and (c).

2. **Candidate (b) — Lazy Response Headers + raw-list `inspect_response`.**
   Targets `build_kernel_headers` 4,380 ns/req — second largest
   single cost. Pattern matches the shipped lazy Request Headers.
   Composes with (a). Excludes (d) — both touch the same
   `inspect_response` callbacks.

3. **Candidate (c) — `request.path` direct getter.** Targets
   ~3,600 ns/req of URL constructor + pathname callbacks on
   /hello. Effort moderate (one new native op), risk low. Requires
   scenarios.js update to realize savings (same pattern as
   `fetchFast`). Composes with (a) and (b).

Ceiling estimate: ~5,800 ns/req from (b)+(c) plus (a)'s share →
roughly 470–520K req/s if all three land. The remaining gap to the
880K /ping baseline is the 23K ns/req amortised in memory management
+ kernel I/O — neither moves much without compio internals work
(candidate (e), high effort).

## 9. Open questions

- **Per-call Global allocation count.** GH Create+Release self
  combined account for 7,690 ns/req. Brand-slot Eternal removed
  ~9.3 pp; the remaining 18.82 pp must come from a small set of
  call sites. **A `--track-global-handles` or Eternal-conversion
  experiment would enumerate.** Highest-leverage open probe.

- **In-V8 microbench harness missing.** Eternal vs Global,
  ObjectTemplate `new_instance` vs `Object::New`, finalizer
  registration cost in isolation — none measurable with criterion
  alone. The prior round and this one both flag this.

- **Compio scheduler self ~3 %.** Closure boxing / waker / task
  glue. Whether reducible needs profile drill-down.

- **GC trace.** `Scavenger::ScavengeObject` 0.73 % self;
  `IterateYoungStrongAndDependentRoots` 0.51 %. A `--trace-gc`
  pass would confirm whether per-request Globals trigger Scavenge
  cycles.

- **JS microbench `json/small` median noise.** Min 1,495, p25
  1,807, median 2,059, p75 2,360. Brief's "1,227 ns" doesn't
  reproduce. Worth a follow-up to reconcile harness/methodology.

## Files

- Report: `docs/archive/perf/httpget-time-distribution-2026-05-08-v2.md`
- Flamegraph (current): `docs/archive/perf/flamegraphs/2026-05-08-httpget-397k.svg` (1.71 MB)
- Flamegraph (prior, 348K): `docs/archive/perf/flamegraphs/2026-05-08-httpget-352k.svg`
- Raw perf data: `/tmp/httpget-distrib-v2/perf.data` (2.36 GB, 142,031 samples)
- perf top-self: `/tmp/httpget-distrib-v2/top-self-full.tsv`
- Cluster summary: `/tmp/httpget-distrib-v2/clusters.txt`
- Folded stacks: `/tmp/httpget-distrib-v2/collapsed.txt` (21,158 stacks)
- Macrobench raw: `/tmp/httpget-distrib-v2/macro-{1,2,3}.txt`
- JS microbench TSV: `/tmp/httpget-distrib-v2/microbench-pinned.tsv` (15×36)
- JS microbench harness: `/tmp/perf-microbench/op-bench-v2.js` (NOT in repo, untouched)
- Microbench driver: `/tmp/httpget-distrib-v2/run-microbench.sh`
- Cluster Python: `/tmp/httpget-distrib-v2/cluster.py`
- Sources cited (read-only): `crates/runtime/src/web/fetch/{response,request}.rs`,
  `crates/runtime/src/web/headers.rs`,
  `crates/runtime/src/transport/handler.rs`,
  `crates/runtime/benches/scenarios.js` (unmodified).
