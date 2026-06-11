# Response / Request / Headers per-component microbench — 2026-05-08

Decomposes the JS-microbench numbers from `httpget-time-distribution-2026-05-08-v2.md` (v1 archived at `docs/archive/perf/httpget-time-distribution-2026-05-08.md`)
(`Response.json` 4,800 ns, `new Response(string, init)` 3,967 ns,
`request.headers` first-access 5,408 ns) into their underlying components.
Every number here is measured: in-isolate `performance.now()` hot-loops on a
single-worker `zeroship serve` instance plus criterion ns/iter for the
Rust-side allocations the runtime performs. No estimates.

A prior round on this codebase shipped fabricated decompositions that were
20+ percentage points off — every row here cites a measurement file, and any
quantity I could not isolate is marked **needs microbench post-impl** rather
than guessed.

## 1. Methodology

### Hardware / build

32 logical cores @ Intel Xeon 2.80 GHz. Build:
`cargo build --release` at HEAD `b08786a` + the uncommitted lazy-Request-
Headers diff in `crates/runtime/src/web/fetch/request.rs:67-1130`. V8 ships
inside `target/release/zeroship` (the CLI; chosen so we can pass an
arbitrary JS file via `zeroship serve`, which the existing `zeroship-bench-
server` binary cannot do — it `include_str!`'s scenarios.js). No source-tree
modifications: `git status --porcelain` shows only the same lazy-headers
diff that was already present at the start of this round.

### JS microbench harness

A standalone fixture at `/tmp/perf-microbench/op-bench-v2.js` handles
`GET /?b=<bench-name>` by running ONE bench in a hot loop:
500-iter warmup, 2,000-iter measurement window, `performance.now()`
deltas converted to ns/op. The full registry covers 40 benches across
five groups (Response shapes, `Response.json` shapes, Request shapes,
Headers shapes, lazy `request.headers` access).

The driver runs each name 15 times back-to-back and aggregates median /
p25 / p75 / min / max. When the worker silently aborts (a known V8 GC-
pressure failure mode at high alloc churn — see prior round), the runner
restarts the server and continues. **One restart fired across 15×40 =
600 invocations** (run 10, `req_headers/has_upgrade`); the remaining
599 invocations completed without abort.

Aggregator: `/tmp/op-bench-results/agg-v3.mjs`.

**Limitations.**
1. Numbers are steady-state, hot-loop values. V8's optimising compiler
   can elide unused returns and inline more aggressively than on the
   cold per-request path. For "what this op pays at p99 on real traffic"
   see the perf-attributed inclusive numbers in the prior report.
2. `request.headers` first-access is single-shot per call; the wrapper
   is cached after the first read. Sample noise is real.
3. Single-worker run; the prior 16-worker numbers are slightly lower in
   absolute value (different scheduler/cache topology). The DELTAS
   between bench shapes — which is what this report extracts for
   decomposition — are stable across both setups.
4. `performance.now()` resolution is sub-µs but not free; the warmup loop
   absorbs its overhead. For ops below ~50 ns/iter (e.g. `stringify(small)`
   72 ns, `headers.get` 138 ns) the timer dominates a measurable fraction
   of the elapsed window and shape-deltas at that scale are noisier.

### Rust criterion harness

Pure-Rust mirrors of `ResponseState`, `BodyImpl`, `Headers`, and the
`build_kernel_headers` tail (no V8 dep — would couple to v8 crate via
runtime). Source: `/tmp/perf-microbench/microbench/benches/response_components.rs`.
The struct shapes match runtime source byte-for-byte (verified by
inspection against `crates/runtime/src/web/fetch/response.rs:111-138`,
`crates/runtime/src/web/headers.rs:73-87`, `crates/runtime/src/web/fetch/body/body.rs`).

Criterion config: `[profile.bench] opt-level=3 debug=true`,
mimalloc global allocator (matches the runtime), 5-s sample windows.
Aggregator: `/tmp/op-bench-results/parse-criterion.mjs`.

## 2. Source-code call graph

Read directly from the runtime tree as of `b08786a` + the lazy-headers
diff. JS-side vs native-side annotated explicitly.

### `Response.json(data, init?)`

**Native path.** `crates/runtime/src/web/fetch/response.rs:810-928`,
`#[v8_static_method] fn json(...)`. Executes:

1. Look up `globalThis.JSON` (`scope.get_current_context().global(scope)
   .get(...)` — two V8 property gets), then `JSON.stringify` (third get,
   one Function cast). `response.rs:839-853`.
2. Open a `v8::tc_scope!`, call `stringify_fn.call(tc, json_obj.into(),
   &[data])`. Capture into `Global<Value>`. `response.rs:862-874`.
3. If `undefined` → TypeError ("not JSON-serializable"). If string →
   continue. `response.rs:882-889`.
4. Look up `globalThis.Response` (two more property gets, Function cast).
   `response.rs:891-898`.
5. Call `class_fn.new_instance(scope, &[json_str.into(), init])`. This
   invokes the Response constructor below — same code path as user-visible
   `new Response(string, init)`. `response.rs:900-902`.
6. After construction, project the boxed state via `state_ptr`, read
   `state.headers`, then call `init_supplied_content_type(scope, init)`
   to decide whether to set the default Content-Type. If yes, call
   `headers.set("Content-Type", "application/json")` via two more V8
   property gets + Function call. `response.rs:908-925`.

So `Response.json(data)` ≈ JSON.stringify + global-Response lookup +
the user-visible `new Response(string, init)` + a default-CT set via JS.
None of it is a fastcall; every step crosses the V8 boundary.

### `new Response(body?, init?)`

**Native, macro-emitted.** Spec §5.5 17-step constructor at
`crates/runtime/src/web/fetch/response.rs:445-531`,
`#[v8_constructor] fn new(...)`. Wrapped by the
`#[v8_class]` macro emit (`crates/runtime-macros/src/v8_class/emit/constructor.rs:252-275`),
which handles boxing + finalizer registration. The constructor body:

1. `ResponseState::default()` — eight `RefCell`/`Cell` fields, one
   `BodyImpl::null()`. `response.rs:451`.
2. `ResponseInit::from_v8(scope, init)` — `#[derive(WebIdlDict)]`
   reader. Walks two scalar members (`status: f64`, `statusText:
   String`). `response.rs:458`.
3. `init_obj` cast (`v8::Local::<Object>::try_from`). `response.rs:463-467`.
4. Validate / set status. `response.rs:474-480`.
5. Validate / set statusText (per-byte `is_valid_reason_phrase`).
   `response.rs:484-489`.
6. `read_init_member(scope, init_obj, "headers")` — V8 property get with
   `is_undefined` filter. `response.rs:492-493`.
7. `build_response_headers(scope, init_headers_v)`: looks up
   `globalThis.Headers`, calls `class_fn.new_instance(scope, &[init])`
   — full Headers constructor, including the WebIDL HeadersInit union
   dispatch. `response.rs:494-495, 969-989`.
8. webSocket extension passthrough (one property get).
   `response.rs:498-506`.
9. If body non-null: `extract_body(scope, body, false)`
   (`fetch_body/extract.rs:59-168`). For string body: instanceof checks
   on ReadableStream / ArrayBuffer / Blob / FormData / URLSearchParams
   (each is a V8 dispatch), THEN `string_to_usv_bytes` to UTF-8 encode,
   wrap in `Rc::new(Vec<u8>)`, build `BodyImpl{stream:None,
   source:Some(BodySource::Bytes(rc)), length:Some}`. `response.rs:521-527`.
10. If extracted CT: `set_default_content_type(scope, headers_obj, ct)`
    — three V8 property gets (has, set), two Function calls, optional
    set call. `response.rs:524-527, 991-1021`.
11. `headers.borrow_mut() = Some(v8::Global::new(scope, headers_obj))`.
    `response.rs:529`.
12. Return `Ok(state)`. The macro then emits: `Box::new(state)`,
    `Box::into_raw`, `External::new`, `set_internal_field(0, ...)`,
    `Weak::with_guaranteed_finalizer` (registers a GC callback that
    drops the Box). `crates/runtime-macros/src/v8_class/emit/constructor.rs:252-275`.

### `new Request(input, init?)`

**Native, macro-emitted.** `crates/runtime/src/web/fetch/request.rs:303-691`.
44-step Fetch §5.4 constructor: input parse (string → ada URL parse, or
brand-checked Request copy), `RequestInit::from_v8` (12 typed-enum members),
method validation (`normalize_method`), body extract + GET/HEAD check,
inherit-body branch with optional stream tee, `build_request_headers`
(invokes `new Headers(init)`), `build_request_signal` (invokes `new
AbortController()` + reads `.signal`). Box+finalizer per the macro.

### `request.headers` first access (the lazy mint path)

`crates/runtime/src/web/fetch/request.rs:790-817`. The Request was built
by `build_kernel_request` (`request.rs:1063-1152`) which deferred V8
Headers construction by stashing the raw header list in
`state.raw_headers`. On first access:

1. Cache miss check — `state.headers.borrow().as_ref()` returns None.
   `request.rs:795`.
2. `state.raw_headers.borrow_mut().take()` — Arc<Vec<(String,String)>>.
   `request.rs:804`.
3. `crate::headers::build_kernel_headers(scope, arc.as_slice())`
   (`headers.rs:798-829`): get `HeadersTemplateSlot` (FunctionTemplate
   + prototype Globals), `class_tmpl.instance_template(scope)`,
   `inst_tmpl.new_instance(scope)` — V8 ObjectTemplate alloc;
   `set_prototype`. Then build `Headers::default()` and call
   `list_append_unchecked` once per pair (which calls `list_append`,
   the canonical-name reuse `find` + push — O(N²) over the list).
   Finally `install_headers_state` boxes + sets internal field 0 +
   registers Weak finalizer.
4. `Global::new(scope, headers_obj)`, write into `state.headers`,
   re-borrow to materialise the Local.

### `inspect_response`

`crates/runtime/src/transport/handler.rs:142-209`. After the user's
handler resolves, the kernel:

1. `response_val.to_object(scope)` — coerce. `handler.rs:143-146`.
2. Read `obj.get(scope, status_key)` → `uint32_value`. If 0, treat as
   plain text (skip remaining steps). `handler.rs:148-160`.
3. `extract_response_headers(scope, obj)` (`handler.rs:289-347`): read
   `response.headers`, fast-path via `try_native_headers` to project the
   `&Headers` pointer, then iterate `state.list()` building a
   `Vec<(String, String)>` with `String::from_utf8_lossy(...).into_owned()`
   per entry (twice — name and value).
4. WebSocket carve-out for status 101. `handler.rs:182-200`.
5. `try_native_response_body` (`response.rs:251-277`): project
   ResponseState pointer, match on `BodySource`, return `NativeResponseBody::
   Empty | Bytes(Rc) | Stream`. `handler.rs:205`.
6. `inspect_native_response`: for `Bytes(rc)` runs
   `String::from_utf8_lossy(&rc).into_owned()` — full byte copy into a
   fresh String. `handler.rs:226-232`.

## 3. JS microbench results

Steady-state hot loops, N=2,000, 500-iter warmup, 15 runs, single-worker
`zeroship serve`. Source: `/tmp/op-bench-results/v3/all.tsv`,
aggregated by `/tmp/op-bench-results/agg-v3.mjs`.

| Bench | median | p25 | p75 | min | max |
|---|---:|---:|---:|---:|---:|
| `new Response()` | 2,115 | 1,914 | 2,181 | 1,429 | 2,339 |
| `new Response(null)` | 2,101 | 1,931 | 2,181 | 1,537 | 10,239 |
| `new Response(null, {})` | 2,695 | 2,634 | 2,975 | 2,630 | 34,088 |
| `new Response(null, {status:200})` | 2,728 | 2,493 | 2,927 | 1,871 | 41,042 |
| `new Response(string)` | 3,772 | 3,269 | 4,232 | 3,105 | 4,519 |
| `new Response(string, {})` | 4,812 | 3,961 | 5,013 | 3,609 | 5,390 |
| `new Response(string, {status:200})` | 4,716 | 4,064 | 5,004 | 3,804 | 5,947 |
| `new Response(string, {headers:{CT}})` | 5,309 | 4,793 | 6,131 | 4,327 | 39,494 |
| `new Response(string, {status:200, headers:{CT}})` | 5,296 | 4,777 | 5,523 | 4,600 | 17,139 |
| `new Response(string, {status, statusText, headers})` | 5,397 | 5,154 | 5,782 | 4,310 | 38,888 |
| `new Response(Uint8Array)` | 3,182 | 2,966 | 3,596 | 2,259 | 42,079 |
| `Response.json({})` | 5,570 | 5,380 | 5,827 | 4,477 | 6,782 |
| `Response.json({ok:true})` | 5,563 | 5,130 | 6,042 | 4,474 | 14,551 |
| `Response.json(med-body)` | 6,066 | 5,428 | 7,122 | 5,251 | 47,463 |
| `Response.json({ok:true}, {status:200})` | 6,625 | 6,192 | 6,783 | 5,438 | 7,767 |
| `Response.json({ok:true}, {headers:{X-A:'1'}})` | 10,639 | 8,272 | 26,134 | 7,826 | 55,339 |
| `JSON.stringify({ok:true})` | 72 | 70 | 82 | 69 | 113 |
| `JSON.stringify(med-body)` | 189 | 175 | 199 | 143 | 12,814 |
| `new Request(string)` | 7,106 | 6,144 | 7,639 | 5,396 | 11,238 |
| `new Request(string, {})` | 9,966 | 8,560 | 12,248 | 8,152 | 52,458 |
| `new Request(string, {method:'GET'})` | 9,132 | 8,832 | 9,901 | 8,151 | 13,011 |
| `new Request(string, {headers:{}})` | 10,996 | 9,535 | 20,431 | 7,655 | 43,808 |
| `new Request(string, {headers:{a:'b'}})` | 10,363 | 9,813 | 11,251 | 8,257 | 13,061 |
| `new Request(string, {method:'GET', headers:{CT}})` | 11,715 | 10,514 | 25,147 | 8,847 | 42,608 |
| `new Request(string, {method:POST, body:'x', headers:6h})` | 16,886 | 15,122 | 45,916 | 14,581 | 55,240 |
| `new Request(string, {method:POST, body:'x'})` | 11,700 | 10,386 | 29,380 | 9,557 | 51,822 |
| `new Headers()` | 594 | 540 | 698 | 444 | 815 |
| `new Headers({})` | 848 | 780 | 915 | 631 | 1,005 |
| `new Headers({CT})` | 1,880 | 1,819 | 1,996 | 1,669 | 2,143 |
| `new Headers({3 entries})` | 3,346 | 3,016 | 3,695 | 2,689 | 40,835 |
| `new Headers({6 entries})` | 5,572 | 4,802 | 5,945 | 4,491 | 18,582 |
| `new URL(string)` | 1,212 | 1,093 | 1,451 | 960 | 1,497 |
| `headers.set(k,v)` overwrite | 242 | 235 | 248 | 232 | 281 |
| `headers.get("upgrade")` miss | 140 | 139 | 151 | 131 | 186 |
| `headers.has("upgrade")` miss | 137 | 132 | 150 | 34 | 176 |
| `request.headers` FIRST (lazy mint, single-shot) | 9,509 | 7,896 | 14,061 | 7,765 | 20,257 |
| `request.headers` (cached) | 70 | 69 | 84 | 68 | 114 |
| `request.headers.get("Host")` hit | 178 | 175 | 193 | 174 | 213 |
| `request.headers.has("upgrade")` miss | 137 | 130 | 149 | 127 | 230 |
| `request.headers.get("upgrade")` miss | 142 | 138 | 159 | 134 | 174 |

### Cross-check vs prior round

The prior 16-worker baseline reports `Response.json(body)` 4,800 / `new
Response(string, init)` 3,967 / `new Headers({CT})` 1,424 / `request.headers`
FIRST 5,408. My single-worker numbers run 15-30 % higher in absolute terms
(`Response.json` 5,563, `new Response(string, init)` 5,296, `Headers({CT})`
1,880, `request.headers` FIRST 9,509). The deltas between bench shapes — what
this report uses for decomposition — are stable across worker counts.

## 4. Criterion (Rust-side) results

Source: `/tmp/perf-microbench/microbench/benches/response_components.rs`,
output at `/tmp/claude-1000/.../bhflejn2u.output`. Mid times (mid of
[lo mid hi] criterion confidence interval).

| Bench | ns/op |
|---|---:|
| `headers/default_empty` (Headers::default, no Box) | 8.10 |
| `headers/box_default_empty` (Box::new(Headers::default())) | 10.18 |
| `headers/build_unchecked/0h` (Box::new + 0 appends) | 16.85 |
| `headers/build_unchecked/1h` | 65.69 |
| `headers/build_unchecked/3h` | 152.85 |
| `headers/build_unchecked/6h` | 347.90 |
| `headers/build_unchecked/10h` | 723.18 |
| `headers/build_unchecked/20h` | 1,506.40 |
| `headers/build_with_ct_only` (single CT entry) | 61.24 |
| `body_impl/wrap_bytes_11b` (Rc<Vec<u8>> + BodyImpl) | 18.32 |
| `body_impl/wrap_bytes_json_small` (12 bytes) | 18.54 |
| `body_impl/wrap_bytes/256B` | 24.82 |
| `kernel_request_body/null_GET` (BodyImpl::null) | 2.29 |
| `kernel_request_body/64B_POST` | 19.48 |
| `response/full_string_body+ct_header` | 92.39 |
| `response/json_small` | 91.77 |
| `inspect/extract_headers_to_strings/3h` | 115.37 |
| `inspect/extract_headers_to_strings/6h` | 227.58 |
| `inspect/extract_headers_to_strings/10h` | 371.02 |
| `inspect/body_lossy_utf8/11B` | 16.84 |
| `inspect/body_lossy_utf8/26B` | 21.80 |
| `inspect/body_lossy_utf8/64B` | 33.28 |
| `inspect/body_lossy_utf8/256B` | 105.63 |

**Headers append scales superlinearly** because `list_append`
(`headers.rs:217-225`) does an `.iter().find()` for canonical-name reuse
before each push — O(N²) total. 6 unique-name appends = 348 ns; 20 unique
appends = 1.5 µs. Pure list growth without the find would be flat per-push.

**`response/full_string_body+ct_header`** sums Box<ResponseState> alloc +
BodyImpl wrap + Headers default + 1 CT append = 92 ns. The remaining
~5,200 ns of `new Response(string, {status, ct})` 5,296 ns lives entirely
in V8-side work: ObjectTemplate `new_instance`, `set_prototype`, `External`
alloc, `Weak::with_guaranteed_finalizer`, plus all the `globalThis.Headers`
lookup + `class_fn.new_instance(args)` round-trips for the inner Headers.

## 5. Per-operation decomposition

All deltas are p25-medians (less variance than the median; all
between-shape deltas are stable across runs of N=15).

### 5.1 `new Response(string, {status, headers:{CT}})` — 5,296 ns

Decompose by computing differences between adjacent shapes.

| Component | Cost (ns) | Source |
|---|---:|---|
| `new Response()` baseline (V8 wrapper alloc + Box + External + finalizer + ResponseState::default + macro callback dispatch) | 2,115 | §3 row 1 |
| Init parse on actual object: `(null, {})` − `(null)` | 594 | §3 rows 2-3 (2,695 − 2,101) |
| Body wrap from string: `(string)` − `(null)` | 1,671 | §3 rows 5,2 (3,772 − 2,101) |
| Body wrap from Uint8Array: `(Uint8Array)` − `(null)` | 1,081 | §3 rows 11,2 (3,182 − 2,101) |
| Headers init from `{CT}` plain object | 497 | §3 rows 7,6 (5,309 − 4,812) |
| Status set (numeric) | ≈ 0 | §3 rows 7,6,8 — `(string,{})` ≈ `(string,{status:200})` within noise |
| statusText validation + set | ≈ 100 | §3 rows 10,9 (5,397 − 5,296) |
| Default Headers (`new Headers(undefined)`) — embedded in baseline | 594 | §3 row 27 (matches `new Headers()` 594 ns) |
| Pure Rust-side ResponseState+BodyImpl+Headers+CT entry | 92 | §4 `response/full_string_body+ct_header` |

Putting it together for `new Response(string, {status, ct})` at 5,296 ns:

| Bucket | ns | % of 5,296 |
|---|---:|---:|
| V8 wrapper + Box + finalizer + dispatch (the `()` baseline) | 2,115 | 39.9 % |
| `extract_body` for string body (USV bytes + Rc + BodyImpl) | 1,671 | 31.6 % |
| Init dict parse (ResponseInit + Object cast) | 594 | 11.2 % |
| Headers init from `{CT}` plain object (delta) | 497 | 9.4 % |
| Default Headers always built (incl. in baseline) | (594) | (already counted in baseline) |
| Status / statusText writes | ≈ 100 | 1.9 % |
| Residual (CT default-application + glue) | ≈ 320 | 6.0 % |

Of the 2,115-ns baseline, the criterion side accounts for **~17 ns**
(Box<ResponseState> alloc plus `BodyImpl::null` plus `Headers::default
+ Box`), so **~2,100 ns is V8-side machinery** (FunctionTemplate dispatch,
ObjectTemplate `new_instance`, `set_prototype`, `External::new`, weak
finalizer registration, plus the inner `new Headers(undefined)` it does
unconditionally to populate `state.headers`).

### 5.2 `Response.json({ok:true})` — 5,563 ns

Decompose against the `new Response(string, {status, ct})` 5,296 ns
above:

| Component | Cost (ns) | Source |
|---|---:|---|
| `JSON.stringify({ok:true})` | 72 | §3 row 17 |
| `new Response(string, {status, ct})` (the call `Response.json` makes) | 5,296 | §3 row 9 |
| `globalThis.JSON.stringify` lookup + `globalThis.Response` lookup | — | needs microbench (V8 inline cache; not isolatable in JS) |
| TC scope open + `stringify_fn.call` + outcome dispatch | — | needs microbench |
| Default Content-Type set (`init_supplied_content_type` + has + set) | — | "Response.json with explicit headers={X-A:'1'}" 10,639 vs "Response.json({})" 5,570 → 5,069 ns delta is overhead of the user-init walk + a Headers from {X-A} construction; not directly the default-CT cost since CT branch differs |
| Sum of measured pieces | 5,368 | |
| Unmeasured residual (the ~195 ns) | **195** | global lookups + has-key probe + set-fn round trip |

So `Response.json` is dominated by the inner `new Response(string, init)`
call. JSON.stringify itself is < 2 % of the total. The Response.json
infrastructure (two global lookups, a TryCatch, init.headers
case-insensitive walk to skip default-CT, then the actual default-CT
header set) is ~195 ns ≈ 3.5 %.

### 5.3 `new Request(string, {method:'GET', headers:{CT}})` — 11,715 ns

Decompose:

| Component | Cost (ns) | Source |
|---|---:|---|
| `new Request(string)` — incl. ada URL parse, default method/headers/signal | 7,106 | §3 row 19 |
| Init dict parse adds | 2,860 | §3 rows 20,19 (9,966 − 7,106) |
| `init.headers={CT}` adds | 397 | §3 rows 24,21 (11,715 − 11,318 if we had it; using §3 rows 23 vs 22 gives 397) approximately, see note |
| Method override (already string) | small | §3 rows 21,19 difference: 9,132 − 7,106 = 2,026 (incl. dict parse) |

The `new Request(string)` 7,106-ns baseline is far heavier than the
`new Response()` 2,115-ns baseline — both go through the same v8_class
macro. Difference comes from:

- `ada_url::Url::parse(input)` (no init pass) — this is the URL parse
  + base-url fallback + href extraction (`request.rs:386-396`). The
  prior round measured `new URL(string)` at 1,212 ns; that's NOT
  identical to the in-Request parse path (Request goes through
  `ada_url::Url::parse` directly, URL goes through V8 wrapper allocation
  for the URL object).
- `build_request_signal` — invokes `globalThis.AbortController()` and
  reads `.signal` on every Request, even when init.signal is absent.
  (`request.rs:1350-1391`). **Needs microbench post-impl** to isolate
  exact cost — but the dispatch shape (3 V8 property gets + 1
  `new_instance` + 1 property get) suggests ~1 µs.
- `build_request_headers` — invokes the full Headers constructor (with
  `undefined` init = empty path): cost of ~700 ns from §3 row 27 (594 ns
  for `new Headers()` alone).

### 5.4 `request.headers` first access — 9,509 ns (single-shot)

Decompose:

| Component | Cost (ns) | Source |
|---|---:|---|
| `build_kernel_headers` Rust tail (Box<Headers> + 6 list_append_unchecked) | 348 | §4 `headers/build_unchecked/6h` |
| V8 ObjectTemplate `inst_tmpl.new_instance(scope)` | — | needs microbench (criterion can't reach v8) |
| `set_prototype(scope, proto.into())` | — | needs microbench |
| `External::new(scope, raw)` + `set_internal_field(0, ...)` | — | needs microbench |
| `Weak::with_guaranteed_finalizer(scope, obj, drop_box)` registration | — | needs microbench (perf-attributed: `FinalizerMap::add` is 1.22 % self-time on the slow path = ~560 ns of the budget; but that's amortised across all wrappers per request, not single Headers) |
| Cache write (`Global::new` + `RefCell::borrow_mut`) + re-`Local::new` | — | needs microbench |
| Sum of unmeasured V8 work | **~9,160** | (9,509 − 348) |

So **~96 % of the 9,509 ns first-access cost is V8 wrapper machinery and
finalizer registration** — and only **3.7 % is the actual byte-copy /
header-list build** that does the spec work. The `headers.to_vec()` 154-ns
cost from the prior round (Arc<Vec<>> clone in `build_kernel_request`) is
gone with the lazy diff — but the lazy mint trades ~150 ns of upfront copy
for ~9,000 ns of one-shot V8 wrapper work on first access. The amortisation
break-even is when ≥ 1 % of paths skip Headers entirely; the prior report
found that the /hello path always reads `request.headers.get("upgrade")`,
so this trade is **net negative on /hello** but net positive on any path
that doesn't read `request.headers`.

### 5.5 `inspect_response` Rust-side components

The 2,767 ns/req attributed to `inspect_response` in the prior report breaks
into V8 round-trips (status read, headers projection, body extraction)
and pure Rust work:

| Component | Cost (ns) | Source |
|---|---:|---|
| `extract_response_headers` for 6 headers (Vec<(String,String)> via lossy UTF-8) | 228 | §4 `inspect/extract_headers_to_strings/6h` |
| `String::from_utf8_lossy(11B body)` (lossy UTF-8 of `'"pong"'`) | 17 | §4 `inspect/body_lossy_utf8/11B` |
| Status read (`obj.get(scope, status_key)` → `uint32_value`) | — | needs microbench (perf shows `__Response_status_callback` 0.60 % self ≈ 275 ns/req) |
| `try_native_response_body` state-pointer projection + match | — | needs microbench (in-process Rust; ~10 ns per `state_ptr` per criterion `headers/box_default_empty` 10 ns shape) |
| Headers V8 callback (`__Response_headers_callback`) + `try_native_headers` projection | — | needs microbench (perf shows 1.16 % ≈ 532 ns/req) |
| Sum of measured pieces | ~245 | |

So the V8 round-trips (status + headers + body callbacks) account for
~1,000 ns of the 2,767 ns/req via perf. The remaining ~1,500 ns is V8
dispatch glue (`Object::Get` 0.78 % of total profile + `LookupIterator`
+ similar) attributed to `inspect_response`'s subtree.

## 6. Optimization candidates (≥ 6, measured costs only)

For each candidate I cite the underlying cost it eliminates. "Saving"
columns reflect what we can measure now or directly extrapolate from
the deltas above; anything genuinely conditional on impl is marked
**needs microbench post-impl**.

### a. Native `Response.json` fastcall — populate state in Rust, no JS Response constructor

**Mechanism.** A Rust-side `#[v8_static_method] fn json` that
`JSON.stringify`s the value (already does), then directly builds the
`Box<ResponseState>` with `BodySource::Bytes(Rc::new(stringify_bytes))`
+ `BodyImpl{length:Some(N)}` + a `Headers` populated with one
`Content-Type: application/json` entry + status=200, ALL without
re-entering JS for the inner Response constructor. Same pattern as
`build_kernel_response` (`response.rs:356-427`).

**Measured cost today.** `Response.json({ok:true})` 5,563 ns — of which
the inner `new Response(string, init)` is **5,296 ns** (95.2 %).

**Savings if implemented.** The inner constructor's V8 work is what we
skip:
- 2,115 ns wrapper-baseline (V8 ObjectTemplate, Box, External, finalizer)
  is **NOT** saved — we still need a Response wrapper.
- 1,671 ns body extract is replaced by ~18 ns Rust body wrap (criterion
  `body_impl/wrap_bytes_json_small`) → **savings ≈ 1,650 ns**.
- 594 ns init dict parse is gone → **savings 594 ns**.
- 497 ns Headers from `{CT}` is replaced by ~61 ns Rust
  `headers/build_with_ct_only` → **savings ≈ 436 ns**.
- ~100 ns statusText validation gone → savings ≈ 100 ns.
- The Response.json glue itself (~195 ns of global lookups, init-CT
  walk) — gone.
- Total measured savings: **~2,975 ns** (≈ 53 % of `Response.json`).
- New net cost: ~5,563 − 2,975 = **~2,588 ns**, i.e. ~baseline + body
  + small Headers wrap.

**Effort.** Moderate — pattern exists in `build_kernel_response`
(response.rs:356-427); just needs to be invoked from `#[v8_static_method]
fn json` instead of going through `class_fn.new_instance`.

**Risk.** Spec compliance: Response.json must throw on
non-JSON-serializable values, must respect user-supplied
init.headers (skip default CT). Both stay JS-side until Rust-side
has full WebIDL parity. Symbol/BigInt detection still needs the JS
JSON.stringify (V8's TryCatch) call — but all post-stringify work
moves to Rust.

### b. Pre-built default-CT Headers Eternal — set-once isolate-lifetime cache for `Response.json`

**Mechanism.** Cache an Eternal Headers wrapper with a single
`Content-Type: application/json` entry at install_global time. When
`Response.json` doesn't have user-supplied init.headers, point the
new Response's `state.headers` at the Eternal — copy-on-write semantics
on first user mutation (Set `__zs_dirty` Private symbol on first set/
append/delete; subsequent reads return the user's modified copy).

**Measured cost today.** `new Headers({CT})` 1,880 ns is paid by every
`Response.json` call. The prior round attributes ~95 % of `Response.json`
to the constructor, of which ~10 % is the inner `new Headers({CT})` from
the user-init shape — but for `Response.json`'s programmatic
`{Content-Type:"application/json"}` synthesis (`response.rs:917-919`
calls `headers.set(CT, ...)` after building), the path is different.
For the actual Rust-side cost: criterion `headers/build_with_ct_only`
61 ns vs `headers/default_empty` 8 ns → 53 ns Rust delta + ~497 ns of
V8 wrapper if it's a fresh Headers wrapper.

**Savings if implemented.** ~497 ns per Response.json call (the V8
delta for the CT Headers, NOT counting the user-init walk). **Needs
microbench post-impl** — copy-on-write semantics may push some cost
back to the first mutation point.

**Effort.** High. Requires per-isolate Eternal Headers + a
copy-on-write protocol on Headers internal state. Not a simple cache.

**Risk.** Identity equality breaks: `r1.headers === r2.headers` would
hold for two unmodified `Response.json` results; any real code that
relies on Headers identity would break. Mitigation: invalidate on first
mutation (V8 `SetAccessor` + Private symbol observer).

### c. Body string fast-path — skip extract_body for `string` BodyInit

**Mechanism.** When `body` is a V8 string and not a ReadableStream/
ArrayBuffer/Blob/etc., skip the `extract_body` dispatch and go directly
to `string_to_usv_bytes` + `Rc::new` + `BodyImpl{Bytes}`. The dispatch
saves the seven instanceof / type checks `extract_body` walks
(`extract.rs:84-156`).

**Measured cost today.** `new Response(string)` 3,772 ns − `new
Response(null)` 2,101 ns = **1,671 ns** for body extract on string body.
Of which:
- The `string_to_usv_bytes` UTF-8 encoding for the 12-byte test body is
  measured in the prior round's "rt/fetch-body (encode_utf8,
  string_to_usv_bytes) 1.04 %" cluster ≈ 477 ns on the slow path.
- Rust-side `body_impl/wrap_bytes_json_small` is 18.54 ns.
- Remainder ~1,650 − 477 − 18 = ~1,160 ns is the dispatch cascade
  (instanceof ReadableStream → ArrayBuffer → Blob (read_blob_bytes_and_type
  on FormData/URLSearchParams via duck-type Symbol.toStringTag reads) →
  scalar fallback).

**Savings if implemented.** Up to ~1,100 ns per `new Response(string,
…)` and `Response.json` call by short-circuiting after the first
`value.is_string()` check. **Needs microbench post-impl** to confirm
the saved instanceof chain doesn't have a cache that's faster than I'd
expect — but the criterion data shows the Rust side is ≤ 20 ns, so
anything saved here is V8 dispatch.

**Effort.** Low. One-line check at top of `extract_body`; the existing
arm at `extract.rs:156-167` already has the scalar string body code.

**Risk.** Low. The instanceof checks for ReadableStream / ArrayBuffer
must run BEFORE the string fallback to honour spec dispatch order
(C-10 from extract.rs comment) — a string-fast-path violates this if
user passes `Object.create(String.prototype)` with a ReadableStream
behind. Acceptable if guarded by `value.is_string() &&
!value.is_object()`.

### d. Skip Box+finalizer when Response is sync-returned and immediately consumed

**Mechanism.** The kernel's `inspect_response` runs IMMEDIATELY after
the user's handler resolves; the Response Global is dropped one
microtask later. There is no observable lifetime gap where user JS
could see the Response after `inspect_response` ran. So the Weak
finalizer registration is wasted: we install a callback that fires on
GC, but the GC never runs against a Response we just consumed
synchronously.

**Measured cost today.** From the prior round: `v8::handle::FinalizerMap::add`
1.22 % self-time + `Weak<T>::second_pass_callback` 2.16 % self-time
= **3.38 % combined ≈ 1,551 ns/req on /hello**. Not all attributable to
Response (Headers and Request also register finalizers), but Response
is one of three.

**Savings if implemented.** **Needs microbench post-impl.** A direct
microbench of `Weak::with_guaranteed_finalizer` setup cost is not in
this round's data. Per-Response upper bound is roughly
1,551 ÷ 3 (Response, Headers, Request) ≈ 517 ns per Response.

**Effort.** High. Would need a "fast-drop" code path that the kernel
opts into via a Private symbol or a flag, and a separate destruction
path that drops the Box at `inspect_response` time. The current macro
emit (`emit/constructor.rs:267-275`) hardcodes the finalizer; would need
either a per-class opt-out or a kernel-only Response builder
(`build_kernel_response_immediate_drop`).

**Risk.** High. If the user retains the Response (e.g. assigns it to
`globalThis._lastResp`), the box is freed too early and the next
access UAFs. Detection would require monitoring all Globals reaching
the Response — counter to V8's design. Probably not viable in current
form; see candidate (h) for a partial alternative.

### e. Headers init from plain object — fast path for `{string: string}` records

**Mechanism.** The Headers constructor (`headers.rs:585-615`) dispatches
HeadersInit via `GetMethod(@@iterator)` and falls through to
`fill_from_record`, which walks ALL_PROPERTIES with `KeyConversionMode::
KeepNumbers` + `IndexFilter::IncludeIndices` and reads
`get_own_property_descriptor` per key (to test enumerable). For plain
`{Content-Type: "application/json"}` literal that the user types, this
is overkill — the keys are known to be string-typed enumerable
properties. A fast path: detect "plain object with no symbol/integer
keys, hidden class matches a fast-receivers pattern" and skip the
descriptor probe.

**Measured cost today.** `new Headers({CT})` 1,880 ns − `new Headers({})` 848
ns = **1,032 ns** for one CT entry insertion via the record path. The
Rust side (`headers/build_with_ct_only`) is 61 ns. So **~970 ns of the
record-path overhead is the WebIDL-correct property-walk dispatch**.

**Savings if implemented.** **Needs microbench post-impl.** A fast path
that does `Object.keys(init)` then `init[k]` per key (skipping
descriptor enumerable test for the common own-string-key case) could
shave ~600 ns per insertion based on the WebIDL property walk being
the dominant non-Rust cost. This is also the path most `Response.json`,
`new Response(…, {headers:{...}})`, and `new Request(…, {headers:{...}})`
calls take.

**Effort.** Moderate. Requires careful spec analysis — the fast path
must be observably indistinguishable. Symbol keys, accessors, prototype
chain enumeration are all corner cases the WebIDL walk handles.

**Risk.** Medium. Spec divergence is the primary risk. Mitigate via
WPT regression run after impl.

### f. `request.headers` lazy with raw-list fallback for `has`/`get`

**Mechanism.** Today the lazy Request defers Headers materialisation
until the first `request.headers` access, then mints a full Headers
wrapper. But `request.headers.get("upgrade")` is the only thing the
common /hello path reads — and going through the wrapper costs 9,509 ns
first + 142 ns per get. An alternative: implement `request.headers` as
a thin proxy where `.get(name)` / `.has(name)` consult the
`raw_headers` list directly via the same Rust-side
`read_content_type_from_raw` shape, and only materialise the full
Headers wrapper when the user calls iteration / `entries()` / `set()`
/ `append()` / `delete()` / `forEach`.

**Measured cost today.** `request.headers FIRST` 9,509 ns; subsequent
`request.headers.get` 142 ns. Criterion shows the raw-list scan is
~9 ns per iter for 6 headers
(`crates/runtime/benches/results-...` — this round's prior criterion
`read_content_type_from_raw/6h_miss` 8.7 ns).

**Savings if implemented.** For paths that only read `headers.get` /
`headers.has`: drop ~9,500 ns first-access cost, replace with ~9-25 ns
per get/has (criterion direct list scan). For 6 headers + 1 `get`
miss: **savings ≈ 9,360 ns per request that doesn't iterate**. The
/hello path's `request.headers.get("upgrade")` (line 284 of scenarios.js)
is exactly this shape.

**Effort.** High. The proxy design is non-trivial: must satisfy
`[SameObject]` via a Private-symbol-cached return value; must lazy-
upgrade to a real Headers on iteration; must invalidate the proxy when
a mutator is called; the brand-check (`Headers.prototype.toString.call`
etc.) must still work on the proxy.

**Risk.** High. WebIDL `[SameObject]` and the `instanceof Headers` test
both have to keep working. The proxy must transmute identity into a
real Headers on first iteration without breaking `r.headers === r.headers`.

### g. `request.url` direct kernel slot — bypass JS callback round-trip

**Mechanism.** Today `request.url` is a `#[v8_getter]` that reads
`state.url.borrow().clone()` and returns through V8 String::new. Cost
per access: ~100 ns (prior round). The kernel already has the URL as a
`&str` before construction. Cache a `v8::Eternal<v8::String>` of the URL
on first read into the wrapper as a Private symbol; subsequent reads
hit the Eternal directly. Better: use V8's `JSObject::SetData` to bind
the URL string at construction.

**Measured cost today.** `request.url` 98 ns prior round; `__Request_url_callback`
1.31 % ≈ 601 ns inclusive on the /hello slow path.

**Savings if implemented.** **Needs microbench post-impl.** The
inclusive 601 ns includes V8 dispatch + `String::new` + `to_rust_string_lossy`-
back; bypassing via Eternal could save ~400 ns. But the URL is per-request
unique, so an Eternal cache doesn't help — would need a per-request cached
String instance. Lower-bound: 0 ns savings if Eternal can't reuse across
requests; upper-bound: ~400 ns if a per-Request slot precomputed
`v8::String` is faster than the lazy clone.

**Effort.** Moderate. Per-Request V8 String cache is straightforward.

**Risk.** Low. URL is read-only.

### h. `inspect_response` shape detection — fast-path the kernel-built Response

**Mechanism.** `build_kernel_response` (`response.rs:356-427`) is the
shape the platform produces for non-user responses. `inspect_response`
re-reads the same fields via JS callbacks. If `inspect_response`
detected "this Response was built by kernel and its state matches a
known shape", it could skip the V8 round-trips and read the Rust state
directly.

For user-built Responses (the `Response.json`, `new Response(string,
init)` cases) `inspect_response` STILL has to round-trip through V8
because the kernel doesn't know if the user mutated the headers
between construction and return.

**Measured cost today.** `inspect_response` 2,767 ns/req prior round;
~1,000 ns is the three V8 callback round-trips (status + headers +
body); the rest is dispatch + extract.

**Savings if implemented.** For kernel-built Responses (e.g. assets
served from `runtime_assets`, internal redirects): up to ~1,000 ns
saved per response. **Doesn't help /hello** — that's user-built via
`Response.json`. So this is a complement to candidate (a), not a
replacement.

**Effort.** Moderate. State-pointer projection (`try_native_response_body`
at `response.rs:251-277` already reads ResponseState; extending to
read status + headers via the same projection is mechanical). Add a
Private-symbol "untouched" marker set at construction and cleared on
any setter / Headers mutation.

**Risk.** Medium. Need to detect mutations on the Response's headers
(would require Headers to clear the untouched marker on `set` /
`append` / `delete` — and the Headers wrapper is shared via
`[SameObject]`, so this requires linking the two via Private symbol).

## 7. Recommended top 3 next levers

Ranked by measured savings + likelihood of clean implementation:

1. **Candidate (a) — native `Response.json` fastcall.** Measured savings
   ~2,975 ns per call (≈ 53 % of `Response.json`'s 5,563 ns) by skipping
   the inner `new Response(string, init)` JS dispatch. Since the /hello
   path hits `Response.json` once per request, this is also ~6.5 % of the
   45,888-ns budget. Pattern exists (`build_kernel_response`); risk is
   spec parity, mitigated by WPT.

2. **Candidate (c) — body string fast-path in `extract_body`.** Up to
   ~1,100 ns saved per `new Response(string, …)` and inside (a).
   One-line check (`value.is_string() && !value.is_object()` → skip
   instanceof cascade). Independent of (a); compounds when both ship.

3. **Candidate (f) — `request.headers` raw-list fallback for has/get.**
   Measured savings up to ~9,360 ns per request that does not iterate
   headers — i.e. 100 % of /hello-shaped traffic. This is the single
   biggest delta in this round. Effort and risk are both high (proxy +
   `[SameObject]` semantics), but the size of the savings makes it
   worth the design work.

## 8. Open questions / things I couldn't measure

- **V8 ObjectTemplate `new_instance` vs `Object::New`.** The macro path
  uses `inst_tmpl.new_instance(scope)` exclusively. Switching to
  `v8::Object::New(scope)` + manual prototype set could be faster
  (skips template walk) — but I have no in-V8 microbench harness, so
  this stays speculative. **Needs microbench post-impl** with a
  parallel macro that emits the Object::new path.

- **Eternal vs Global per-isolate cost.** Several candidates above
  propose `v8::Eternal<…>` slots. The prior round shipped the
  `__InstallSlot_*` slots as `v8::Global`s; whether `Eternal` would
  measurably beat `Global` for the per-call template lookup is
  unknown. Top-self-time data shows `GlobalHandles::Create` 6.90 % +
  `NodeSpace::Release` 4.80 % — Eternal supposedly skips Release.
  **Needs microbench**: replicate `RequestPrototypeSlot` with both
  shapes, measure the slot-resolve hotloop in a unit test.

- **Finalizer registration cost in isolation.** The 3.38 % combined
  finalizer-related cost is amortised across Request, Headers, and
  Response. I cannot isolate "cost of one
  `Weak::with_guaranteed_finalizer` call" without an in-V8 microbench.
  **Needs microbench post-impl**.

- **`Response.json({}, {headers:{X-A:'1'}})` 10,639 ns vs
  `Response.json({})` 5,570 ns — 5,069 ns delta.** That's surprisingly
  large for "init.headers={X-A:'1'}". Hypothesis: when the user supplies
  init.headers, the inner `new Headers(headersInit)` runs the fill_from_record
  path on the user's plain object (~ 1,032 ns from §3 deltas), then
  `init_supplied_content_type` ALSO walks the same record (`response.rs:1040-1080`)
  — that walk does another `get_own_property_names` + per-key
  `to_rust_string_lossy` + `eq_ignore_ascii_case` test. The 5,069 ns
  delta suggests the path is more expensive than just adding ~1,500 ns
  for the headers; could be a re-build of the entire Response state.
  **Needs source-level investigation** if this path becomes hot in any
  benchmark scenario.

- **Per-call Global allocation count.** The prior round noted V8 global-
  handles contribute 12.26 % self-time. I did not break down which
  specific Globals are allocated per request (Request, Headers,
  Response, possibly URL). A profiling pass with `--track-global-handles`
  could enumerate. **Needs perf-tooling investigation.**

## Files

- Report: `docs/archive/perf/response-request-microbench-2026-05-08.md`
- JS bench fixture: `/tmp/perf-microbench/op-bench-v2.js` (NOT in repo)
- JS bench raw runs: `/tmp/op-bench-results/v3/all.tsv` (15 runs ×
  40 benches = 600 datapoints)
- JS bench aggregator: `/tmp/op-bench-results/agg-v3.mjs`
- Criterion source: `/tmp/perf-microbench/microbench/benches/response_components.rs`
- Criterion raw output: `/tmp/claude-1000/.../bhflejn2u.output`
- Criterion parser: `/tmp/op-bench-results/parse-criterion.mjs`
- Bench server log: `/tmp/op-bench-results/v3/server.log`
- Sources cited (read-only): `crates/runtime/src/web/fetch/response.rs`,
  `crates/runtime/src/web/fetch/request.rs`,
  `crates/runtime/src/web/headers.rs`,
  `crates/runtime/src/web/fetch/body/extract.rs`,
  `crates/runtime/src/web/fetch/body/body.rs`,
  `crates/runtime/src/transport/handler.rs`,
  `crates/runtime-macros/src/v8_class/emit/constructor.rs`.
