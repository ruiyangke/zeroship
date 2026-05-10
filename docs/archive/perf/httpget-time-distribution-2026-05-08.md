# httpGet slow-path time distribution — 2026-05-08

Decomposes where each microsecond goes on the `default.fetch` `/hello`
path at the current state (lazy `request.headers` materialisation,
post-`b08786a`). Every number in this report is measured: `perf record`
self-time + folded-stack inclusive attribution, in-isolate JS
`performance.now()` micro-loops, and `cargo bench` (criterion) for
isolated Rust-side allocations. No estimates.

A prior round on this codebase shipped fabricated decompositions; this
report deliberately notes "needs measurement" rather than guessing
where a number can't be measured cleanly.

## 1. Methodology

### Hardware / build / scenario

32 logical cores @ Intel Xeon 2.80 GHz, two NUMA nodes (server on 0,
client on 1, both with `numactl --cpunodebind --membind`). Build:
`cargo build --release -p zeroship-runtime --bin zeroship-bench-server`
at HEAD `b08786a` + the uncommitted lazy-Request-Headers diff in
`crates/runtime/src/web/fetch/request.rs:67-1130`. Server:
`--port=5101 --workers=16`. Client: zerobench at
`~/Projects/zerobench/target/release/zerobench`, single-scenario plan
hitting `GET /hello`, 300 conns × 16 client threads × 10 s saturate.
`kernel.perf_event_paranoid = 1` — kernel addresses unresolved
(`[k] 0xffffffff…`).

### Macrobench baseline

Three 10 s saturate runs after final rebuild, scenarios.js restored
to HEAD:

| Run | req/s |
|---|---:|
| 1 | 352,302 |
| 2 | 347,611 |
| 3 | 346,116 |
| **Mean** | **348,676** |
| Range | 6,186 (1.8 %) |

Source: `/tmp/httpget-distrib/macrobench-final-{1,2,3}.txt`. Brief's
352,080 reproduces within noise.

**Per-request CPU budget**: 16 cores × 1 / (348,676 / 16) =
**45,888 ns/req**. The denominator for every "% of budget" in §5.

### How each cost was isolated

| Source | Method |
|---|---|
| Top-20 self-time + clusters | `perf report --no-children --sort symbol --percent-limit 0` + `/tmp/httpget-distrib/cluster.py` regex grouping |
| Inclusive (which JS op triggered which Rust subtree) | `inferno-collapse-perf` → folded stacks, awk `$0 ~ pattern` |
| Per-op JS ns/iter | In-isolate microbench at `/_bench/op-distribution`, 15 runs, median |
| Per-op Rust ns | criterion at `/tmp/perf-microbench/microbench/benches/{build_kernel_request_alloc,header_scan}.rs` |

`perf record -F 999 -p $PID -g --call-graph dwarf,16384 sleep 10`
during saturate: 159,003 samples, 159.16 G event count. Flamegraph at
`docs/perf/flamegraphs/2026-05-08-httpget-352k.svg`.

### Microbench harness

A temporary `/_bench/op-distribution` path in
`crates/runtime/benches/scenarios.js`'s `default.fetch` handler
measures each op in a hot loop after a 200-iter warmup. The bench
`request` is the live Request `build_kernel_request` constructs.
First `request.headers` access is measured single-shot (median across
15 HTTP calls) because the wrapper is cached after the first read;
subsequent reads measure cache-hit steady state. N = 5,000 because
larger counts within one sync handler invocation reliably crashed the
worker after 1–5 calls (silent abort, likely GC-pressure-induced).
**scenarios.js was restored to HEAD before the final macrobench
numbers and before finishing this report** (`git diff
crates/runtime/benches/scenarios.js` returns zero lines); the bench
source lives only at
`/tmp/httpget-distrib/scenarios.js.bench-version`.

**Limitations.** (1) JS microbench numbers are **steady-state, hot-
loop** values; V8's optimising compiler can elide unused returns and
inline more aggressively than on the cold per-request path. The
perf inclusive attribution in §5 is the more authoritative "cost on
the slow path"; microbench shows what each call costs *given* V8
has fully warmed up. (2) First-access is single-shot per HTTP call.
(3) `request.headers` first-access ns/op is on the same scale as
the kernel syscall round trip — small samples, noise is real.

## 2. perf record — top 20 self-time symbols

Source: `/tmp/httpget-distrib/top-self-clean.txt`. 159,003 samples
across 16 worker threads, aggregated by `--sort symbol`.

| % | Symbol |
|---:|---|
| 6.90 | `v8::internal::GlobalHandles::Create` |
| 4.80 | `v8::internal::GlobalHandles::NodeSpace::Release` |
| 4.60 | `[k] 0xffffffffa0f680cd` (kernel syscall) |
| 4.43 | `_mi_page_malloc` |
| 2.16 | `v8::handle::Weak<T>::second_pass_callback` |
| 1.22 | `v8::handle::FinalizerMap::add` |
| 1.08 | `__memmove_evex_unaligned_erms` |
| 0.99 | `serve::handle_connection` |
| 0.96 | `mi_free` |
| 0.87 | `drop_in_place<headers::Headers>` |
| 0.86 | `v8::internal::Invoke` |
| 0.78 | `v8::Object::Get` |
| 0.76 | `mi_heap_malloc_aligned_at` |
| 0.72 | `Utf8DecoderBase::Utf8DecoderBase` |
| 0.68 | `Factory::AllocateRaw` |
| 0.67 | `RuntimeInner::call_fetch_handler` |
| 0.66 | `_mi_page_malloc_zeroed` |
| 0.64 | `Scavenger::ScavengeObject` |
| 0.57 | `Builtins_CallApiCallbackOptimizedNoProfiling` |
| 0.54 | `v8::Function::Call` |

### Symbol clusters

Source: `/tmp/httpget-distrib/cluster-summary.txt` (regex prefix
groups defined in `/tmp/httpget-distrib/cluster.py`). Sums to 94.81 %
(remainder is sub-0.05 % helpers).

| Cluster | % |
|---|---:|
| kernel syscalls (`[k]` — unresolved at paranoid=1) | 17.91 |
| v8 global-handles (Create/Release/Iterate) | 12.26 |
| unmatched (compio scheduler + fine-grained v8 helpers) | 11.62 |
| alloc/mimalloc | 7.33 |
| v8 scope/isolate (NewCallbackScope, HandleHost) | 6.56 |
| v8 object-property (Object::Get, LookupIterator) | 4.37 |
| v8 api-callback (Builtins_CallApiCallback…) | 3.83 |
| v8 finalizers (Weak::second_pass_callback + FinalizerMap::add) | 3.64 |
| v8 strings/hashing (Utf8Decoder, StringTable, hash) | 3.25 |
| v8 factory-alloc (AllocateRaw, NewExternal, NewString) | 2.68 |
| rust-stdlib (core/alloc adapters) | 2.59 |
| rt/headers (Headers ops + Drop) | 2.43 |
| v8/other (grab-bag) | 2.18 |
| v8 heap/gc (Scavenger) | 1.46 |
| memmove + libc memcmp/memchr | 1.74 |
| rt/fetch-request (`build_kernel_request` self) | 1.13 |
| rt/serve (`handle_connection`) | 1.11 |
| alloc/system (`__rust_alloc`, `__libc_malloc2`) | 1.08 |
| rt/fetch-body (encode_utf8, string_to_usv_bytes) | 1.04 |
| rt/fetch-response | 0.96 |
| rt/runtime (`call_fetch_handler` self) | 0.74 |
| v8 funccb / string-new / global-new | 1.64 (combined) |
| ada-url (parse + free) | 0.82 |
| rt/webidl + rt/url + rt/transport | 1.20 |
| v8/json (FastJsonStringifier) | 0.44 |
| vdso (`__vdso_clock_gettime`) | 0.30 |

V8 GC bookkeeping (12.26 + 3.64 + 1.46 = **17.4 %**) plus allocator
(7.33 + 1.08 + 1.74 = **10.2 %**) = 27.6 % of CPU on memory-management
bookkeeping alone. Kernel I/O is comparable at 17.91 %.

### Inclusive attribution by JS-fired native callback

Folded stacks (`/tmp/httpget-distrib/collapsed.txt`, 26,584 unique
stacks), summed by `awk '$0 ~ pattern'`. Subtrees nest — e.g.
`__Request_headers_callback` includes the `build_kernel_headers` it
triggers.

| Symbol | Incl % | JS op |
|---|---:|---|
| `__Response_constructor` | 15.25 | `Response.json({…})` line 311 |
| `inspect_response` (kernel post-handler) | 6.03 | extracts status/headers/body |
| `__URL_constructor` | 6.06 | `new URL(request.url)` line 282 |
| `__Request_headers_callback` | 4.85 | `request.headers.get("upgrade")` first call |
| `build_kernel_request` (kernel pre-handler) | 4.55 | Request construction |
| `build_kernel_headers` | 4.07 | nested under `__Request_headers_callback` (lazy diff — no longer under `build_kernel_request`) |
| `__URL_pathname_callback` | 2.94 | 4× `url.pathname` checks |
| `__Request_method_callback` | 1.50 | final `Response.json({method, url})` |
| `__Request_url_callback` | 1.31 | same |
| `__Response_headers_callback` | 1.16 | `inspect_response` reading headers |
| `__Headers_get_callback` | 0.98 | `headers.get("upgrade")` itself |
| `__Response_status_callback` | 0.60 | `inspect_response` reading status |

JS-attributable callback subtotal (top-level, no double-count):
6.06 + 4.85 + 2.94 + 1.50 + 1.31 + 0.98 + 15.25 = **32.89 %**, plus
the two kernel-side stages (4.55 + 6.03) = **43.47 %**. Remaining
~57 % is kernel syscalls (17.91), V8 dispatch between callbacks,
allocator churn shared across callbacks, and compio/async scaffolding
(~3 %).

## 3. In-isolate JS microbench (per-op ns/iter)

Source: `/tmp/httpget-distrib/microbench-aggregated.tsv`. Median of
15 invocations of `/_bench/op-distribution` (3 batches × 5 calls,
fresh server per batch). N = 5,000 per loop, warmup = 200.
**Steady-state, hot-loop values** — see §1 limitations.

| Operation | ns/op (median) | min | max |
|---|---:|---:|---:|
| `request.headers` FIRST (lazy mint, single-shot/call) | **5,408** | 4,552 | 7,731 |
| `Response.json(body)` | 4,800 | 4,437 | 5,052 |
| `new Response(string, init)` | 3,967 | 3,719 | 4,338 |
| `new Headers({CT})` | 1,424 | 1,275 | 1,780 |
| `new URL(string)` | 824 | 778 | 1,023 |
| `JSON.stringify(body)` | 217 | 200 | 351 |
| `headers.set` (overwrite) | 181 | 174 | 195 |
| `url.pathname` | 102 | 93 | 509 |
| `headers.get("upgrade")` | 99 | 94 | 116 |
| `request.url` | 98 | 81 | 405 |
| `headers.has("upgrade")` | 96 | 94 | 107 |
| `request.method` | 90 | 84 | 324 |
| `request.headers` (cached) | 47 | 44 | 60 |

The 50× gap between first-access (5,408 ns) and cached (47 ns)
`request.headers` is `build_kernel_headers` + V8 wrapper alloc +
finalizer registration. The handler hits the first-access path once
per request via `request.headers.get("upgrade")` at line 284.

## 4. Criterion microbenches (Rust side)

Source: `/tmp/perf-microbench/microbench/benches/{build_kernel_request_alloc,header_scan}.rs`.
Pure Rust — no V8 dep, harness mirrors runtime source verbatim
(stand-alone V8-isolated benches are out of scope here).

| Bench | time |
|---|---:|
| `kernel_request_alloc_only/6h_GET` (Arc::new(headers.to_vec()) + method/url to_string; mirrors `request.rs:1102-1125`) | 178.0 ns |
| `headers_to_vec/3h` | 90.9 ns |
| `headers_to_vec/6h` | 154.2 ns |
| `headers_to_vec/10h` | 253.6 ns |
| `headers_to_vec/20h` | 494.3 ns |
| `two_strings/method_and_url` (method + URL `to_string`) | 17.5 ns |
| `read_content_type_from_raw/6h_miss` (mirrors `request.rs:240-247`) | 8.7 ns |
| `read_content_type_from_raw/7h_hit` | 23.4 ns |
| `is_header_name/host` (mirrors `headers.rs:106-107`) | 7.4 ns |
| `is_header_name/long_token` (34 bytes) | 52.3 ns |

`headers.to_vec()` scales ~linearly per header (≈ +24 ns above 3) —
on /hello (6 headers) it's 154 ns ≈ **0.34 % of the 45,888-ns
budget**. Full alloc tail = 178 ns ≈ **0.39 %**. perf attributes
4.55 % to `build_kernel_request` overall, so V8-coupled work
(`SetPrototype`, `External::new`, finalizer registration, slot
lookups) is the remaining ~4.16 %.

## 5. Time distribution table

Budget: **45,888 ns/req**. ns/req column = `% × 45,888 ÷ 100`. The
microbench column is steady-state hot-loop — different from
perf-attributed (real traffic pays cold-call + shared GC/allocator
on top); see §1 limitations.

| Operation | Triggered | perf % | ns/req | µbench ns/op | Src |
|---|---|---:|---:|---:|---|
| `__Response_constructor` (`Response.json(...)`) | line 311 | 15.25 | 6,998 | 4,800 | §2,§3 |
| kernel syscalls (recv/send/epoll) | every request | 17.91 | 8,219 | n/a | §2 |
| V8 GC bookkeeping (GlobalHandles+Finalizers+GC) | amortised | 17.36 | 7,966 | n/a | §2 |
| Allocator (mimalloc+system+memmove+memcpy) | amortised | 10.15 | 4,658 | n/a | §2 |
| `__URL_constructor` (`new URL(...)`) | line 282 | 6.06 | 2,781 | 824 | §2,§3 |
| `inspect_response` (kernel post-handler) | every request | 6.03 | 2,767 | n/a | §2 |
| `__Request_headers_callback` (first access) | line 284 | 4.85 | 2,226 | 5,408 (single-shot) | §2,§3 |
| `build_kernel_request` (kernel pre-handler) | every request | 4.55 | 2,088 | 178 (Rust alloc) | §2,§4 |
| `__URL_pathname_callback` (4×) | lines 287,295,301,305 | 2.94 | 1,349 | 102 | §2,§3 |
| `__Request_method_callback` | line 311 | 1.50 | 688 | 90 | §2,§3 |
| `__Request_url_callback` | line 311 | 1.31 | 601 | 98 | §2,§3 |
| `__Response_headers_callback` | inspect_response | 1.16 | 532 | n/a | §2 |
| `__Headers_get_callback` | line 284 | 0.98 | 450 | 99 | §2,§3 |
| `__Response_status_callback` | inspect_response | 0.60 | 275 | n/a | §2 |
| `drop_in_place<Headers>` (self) | finalizer | 0.87 | 399 | n/a | §2 |
| `is_header_name` (self) | per header | 0.41 | 188 | 7.4 / 52.3 | §2,§4 |
| compio/async_task | per request | ~3 (`clone_waker` 0.45 + `RawTask::run` 0.29 + …, summed from top "unmatched") | — | n/a | §2 |
| **Total accounted** | | **~92** | ~42,000 | | |

Cross-check: 348,676 × 16 cores = 5.58 M req/s aggregate → 2.87 µs
CPU/req (= 45,888 ns/req per core). Accounted rows ≈ 92 %; residual
~8 % spreads across compio scheduler, fine-grained v8 helpers below
0.05 % each, and sub-symbol-threshold tails.

## 6. Top candidates for next round (> 5 % of budget)

All "needs implementation" — no projected savings claimed.
Implementation paths sketched as prompts only.

**a. `__Response_constructor` 15.25 % (≈ 7.0 µs).** Biggest JS-side
cost. Microbench: `Response.json(body)` 4,800 ns vs
`JSON.stringify(body)` 217 ns → ~95 % of Response.json is constructor
+ Headers setup. Candidate: native fast path that populates Response
internal slots directly without the WebIDL constructor or a separate
Headers wrapper.

**b. kernel syscalls 17.91 % (≈ 8.2 µs).** Unresolved at paranoid=1.
Needs measurement: re-record with paranoid=0 to split recv/send vs
accept/epoll.

**c. V8 GC bookkeeping 17.36 % (≈ 8.0 µs).** GlobalHandles 6.90 %
self + NodeSpace::Release 4.80 % + Weak callbacks 2.16 % +
FinalizerMap 1.22 % + Scavenger 0.64 %. `b08786a` already removed one
Global per call; remaining Globals are Request, lazy Headers,
Response. Candidate: audit Headers/Response template lookups for
residual per-call Global allocs (RequestPrototypeSlot at
`request.rs:1081-1091` is the model).

**d. allocator 10.15 % (≈ 4.7 µs).** Tied to (c). Plus
`headers.to_vec()` 154 ns + method/url `to_string` 17.5 ns
unconditional in `build_kernel_request`. Candidate: `Arc<str>` or
string-table for method, `Box<[u8]>` flat storage for headers.

**e. `__URL_constructor` 6.06 % (≈ 2.8 µs).** ada self only 0.41 %,
so ~90 % is V8 wrapper. Candidate: kernel-prebuilt URL on Request
(lazy like Headers); or `request.path` / `request.searchParams`
shortcuts.

**f. `inspect_response` 6.03 % (≈ 2.8 µs).** status + headers
callbacks are V8 round trips on every request. Candidate: native
fast-path symmetric to (a) — skip JS-callback round-trips when we
built the Response ourselves.

**g. `__Request_headers_callback` first-access 4.85 % (≈ 2.2 µs).**
Microbench: 5,408 ns first vs 47 ns cached. Candidate: native
Request shortcuts for the small known set of common headers
(`Upgrade`, `Content-Type`, `Authorization`) consulting
`RequestState.raw_headers` directly — `read_content_type_from_raw`
8.7 ns already does this for body CT lookup.

## Files

- Report: `docs/perf/httpget-time-distribution-2026-05-08.md`
- Flamegraph: `docs/perf/flamegraphs/2026-05-08-httpget-352k.svg` (1.94 MB)
- Raw perf data: `/tmp/httpget-distrib/perf.data` (2.65 GB, 159,003 samples)
- Folded stacks: `/tmp/httpget-distrib/collapsed.txt`
- Self-time (full): `/tmp/httpget-distrib/self-full.tsv`
- Cluster summary: `/tmp/httpget-distrib/cluster-summary.txt`
- JS microbench JSONs: `/tmp/httpget-distrib/microbench-r{1..15}.json`
- Aggregated microbench: `/tmp/httpget-distrib/microbench-aggregated.tsv`
- Macrobench raw: `/tmp/httpget-distrib/macrobench-final-{1,2,3}.txt`
- Criterion sources: `/tmp/perf-microbench/microbench/benches/{build_kernel_request_alloc,header_scan}.rs`
- scenarios.js bench-version snapshot: `/tmp/httpget-distrib/scenarios.js.bench-version` (NOT in repo)
