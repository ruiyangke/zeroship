# zeroship runtime — stability audit (2026-04-19)

**Scope:** can the runtime crash during benchmarks or production load? Which risks are real vs theoretical, which are fixed vs latent.

**Verdict:** stable enough for the 100-second benchmark we just ran twice — but three small real bugs surfaced in this audit, plus several untested risk areas. None are immediate crash-on-next-request. All are bounded; recommended fixes below.

---

## How this was verified

Every claim here is backed by one of:
- **[code]** — direct grep/read of source.
- **[probe]** — a live HTTP request against a spawned runtime instance.
- **[soak]** — a timed load test with RSS sampling.
- **[bench]** — yesterday's two consecutive 10s×10 scenario runs.
- **[untested]** — claim I did not actually verify in this audit.

---

## Executive summary

| Risk | Severity | Status |
|---|---|---|
| **B1** `next_stream_id: u32` wraps at 4.29B | Medium | **confirmed [code]** |
| **B2** Detached-task panics are silently swallowed | Medium | **confirmed [code]** |
| **B3** Non-array args body silently treated as empty | Low | **confirmed [probe]** |
| **B4** Malformed JSON returns HTTP 500 instead of 400 | Low | **confirmed [probe]** |
| **B5** Internal parser message leaked in error body | Low | **confirmed [probe]** |
| **B6** `unsafe_op_in_unsafe_fn` warning at `runtime.rs:638` | Low | **confirmed [code]** |
| **R1** V8 heap-limit callback firing mid-dispatch | Medium | untested |
| **R2** compio io_uring cancellation races under load | Medium | untested |
| **R3** RefCell double-borrow panics under HMR + concurrent requests | Medium | untested |
| **R4** Stream cleanup on client TCP-RST disconnect | Medium | untested |
| **R5** Pool × streaming interaction under load | Medium | untested |

Plus **1 positive finding**: 90-second soak shows no memory leak.

---

## Confirmed bugs

### B1. `next_stream_id: u32` wraps silently at 4.29B requests

**Evidence [code]:**
```rust
// crates/runtime/src/state.rs:219
pub next_stream_id: u32,

// crates/runtime/src/state.rs:306
next_stream_id: 1,

// crates/runtime/src/fetch.rs:277-278
let sid = s.next_stream_id;
s.next_stream_id += 1;

// crates/runtime/src/streams.rs:67-68
let id = s.next_stream_id;
s.next_stream_id += 1;
```

Two sites bump the counter, both with plain `+=`. Release builds wrap on overflow.

**Impact:** at 100 k fetch/stream operations per second per worker, wrap happens at ~12 hours. After wrap, IDs collide with long-running streams whose original resolvers are still in `pending_resolvers`. The JS promise system picks up whatever resolver the colliding ID points to — **silent cross-wiring of two requests**. Not a crash; a correctness bug at extreme load.

**Fix cost:** change type to `u64` (4 LOC). Covers ~292 billion years of load even at 1 B ops/sec.

### B2. Detached-task panics are silently swallowed

**Evidence [code]:**
```
crates/runtime/src/fetch.rs:581    .detach()    // spawn_body_reader, per-fetch
crates/runtime/src/runtime.rs:752  .detach()    // pump_loop, per-runtime
crates/runtime/src/serve.rs:1403   .detach()    // handle_connection, per-TCP-conn
```

No `catch_unwind` anywhere in `crates/runtime/src/`. No panic hook. No `set_hook`.

**Impact:** compio's detached tasks catch panics at the runtime boundary and silently discard them. A `.unwrap()` on a race condition in `spawn_body_reader`:
- Connection drops silently (no log).
- `stream_id` slot is leaked in `state.streams` (never closed).
- Observability: zero signal to operators.

A panic in `pump_loop` would kill the event loop for the entire worker. The dispatch thread would then hang waiting for pump events. Requests would time out. **This IS the slow-crash failure mode.**

**Fix cost:** wrap each detached task body in `std::panic::catch_unwind` and `eprintln!` the panic before returning. ~30 LOC across 3 sites.

### B3. Non-array args body silently treated as empty

**Evidence [probe]:**
```
$ curl -s -X POST http://localhost:5200/_rpc/ping \
       -H 'Content-Type: application/json' \
       -d '{"not":"array"}'
"pong"
HTTP 200
```

The RPC wire contract says body is a JSON array of args. Sending `{"not":"array"}` silently invokes the handler with zero args. Ping happens to accept zero args, so it returns "pong" as if nothing were wrong.

A handler like `function divide(a, b) { return a / b; }` would be called with `undefined / undefined = NaN` and silently return NaN. Very surprising behavior — violates the principle of loud failure.

**Location:** `sdks/vite-plugin/src/dev-bootstrap/index.ts` around line 190; similar path in Rust-side dispatch.

**Fix cost:** reject non-array bodies with HTTP 400 `{"message":"RPC args body must be a JSON array"}`. 5 LOC.

### B4. Malformed JSON returns HTTP 500 instead of 400

**Evidence [probe]:**
```
$ curl -s -X POST http://localhost:5200/_rpc/ping \
       -H 'Content-Type: application/json' \
       -d '{broken json'
{"message":"Expected property name or '}' in JSON at position 1 (line 1 column 2)","name":"Error"}
HTTP 500
```

Malformed input from the client is a 4xx (Bad Request), not a 5xx (Server Error). Load balancers distinguish these; client libraries retry on 5xx but not 4xx.

**Fix cost:** classify JSON parse errors as 400 in the dispatcher. 3 LOC.

### B5. Internal parser message leaked in error body

Same curl output as B4: the server echoes V8's exact parser diagnostic (`"Expected property name or '}' in JSON at position 1 (line 1 column 2)"`). Minor information disclosure — not a security incident, but unnecessary.

**Fix cost:** strip internal details in error responses. Preserve in server logs. 5 LOC.

### B6. `unsafe_op_in_unsafe_fn` warning at `runtime.rs:638`

**Evidence [code]:**
```
warning[E0133]: dereference of raw pointer is unsafe and requires unsafe block
   --> crates/runtime/src/runtime.rs:638:32
    |
638 |             let counter = &mut *(data as *mut u32);
    |                                ^^^^^^^^^^^^^^^^^^^ dereference of raw pointer
```

Inside `near_heap_limit_callback`. The function is declared `unsafe extern "C"` but Rust 2024 requires each unsafe operation to be explicit. The fix is trivial (wrap the deref in `unsafe { }`) but forces a re-audit of what's actually being done: a raw pointer to `counter: u32`, passed through V8's callback data, dereferenced without a null check and without guarantee of exclusive access.

**Real concern, not just a lint:** if V8 ever fires this callback from a thread other than the isolate thread (it shouldn't, but it's a C callback from V8 internals), `&mut *(data)` is UB.

**Fix cost:** explicit unsafe block + null check + comment explaining the thread invariant. 10 LOC.

---

## Positive finding — no memory leak under 90s load

**Evidence [soak]:**

Single-worker zeroship serving `ping`, driven by a curl pipeline for 90 seconds:
```
PID=1551233, baseline RSS 22.1 MB

t=18s  RSS=25.5 MB
t=36s  RSS=25.8 MB
t=54s  RSS=26.0 MB
t=72s  RSS=25.6 MB   (dropped — GC or freed buffers)
t=90s  RSS=25.8 MB
```

RSS oscillates between 25.5 and 26.0 MB. **Not monotonically growing.** V8 GC reclaims. No leak on the ping hot path over 90 s.

Caveats:
- Single-connection curl is weak load (~10k req/s, not 1 M).
- Only ping exercised; fetch/streaming paths not soaked.
- 90 s is too short to catch slow leaks.

---

## Adversarial input probe

**Evidence [probe]:** 8-shot test against a running single-worker runtime, all with `curl --max-time 2`:

| # | Input | Expected | Got | Result |
|---|---|---|---|---|
| 1 | valid `POST /_rpc/ping` with `[]` | 200 "pong" | 200 "pong" | ✅ |
| 2 | malformed JSON `{broken json` | 400 | **500** | ❌ B4 |
| 3 | non-array body `{"not":"array"}` | 400 | **200 "pong"** | ❌ B3 |
| 4 | nonexistent method | 404 | 404 | ✅ |
| 5 | GET on POST-only | 404/405 | 404 | ✅ |
| 6 | 100 MB body | 413 | 413 | ✅ (cap works) |
| 7 | 1 MB valid args (string) | 200 echo | 200 echo | ✅ |
| 8 | ping after all the above | 200 | 200 | ✅ (still alive) |

**Runtime did not crash through any of those paths.** Good for benchmark stability. Three real contract violations (B3, B4, B5) and five correct behaviors.

---

## Untested risks (honest to-do list)

### R1. V8 heap-limit callback firing mid-dispatch
`near_heap_limit_callback` fires after 5 near-limit hits, calling `isolate.terminate_execution()`. The dispatch path has multiple V8 property accesses (`classify_fulfilled`, `extract_settled_result`) that could race with termination.

**How to test:** configure `--heap-limit-mb=32`, run the bench. Memory pressure should eventually fire the callback; observe whether the runtime survives.

### R2. compio io_uring cancellation races
Our own earlier finding: "cancellation is not reliable" — dropped read futures may have kernel-side completions that discard bytes. Partially mitigated in connection.rs; not re-audited for the async-generator wrap path added in Solution 1.

**How to test:** run the bench with `cpu_limit` set aggressively (e.g. 5ms) so more dispatches get cancelled mid-flight. Watch for stream-id leaks in `state.streams` over time.

### R3. RefCell double-borrow panics under HMR
`Runtime` holds `Rc<RefCell<RuntimeInner>>`. Every public method borrows. If a V8 callback re-enters during `borrow_mut()`, panic. Solution 1 added new V8 callback points.

**How to test:** run `vite dev` with a demo. Edit `src/index.ts` while a request is in flight. Repeat 100 times. Watch for panics.

### R4. Stream cleanup on client TCP-RST
SSE streams register a `stream_id` in `state.streams`. The close lifecycle was fixed (`eb48a82`) for the happy path. Not verified for abrupt client disconnect.

**How to test:** open an SSE connection, send one RST, observe whether `stream_id` is released.

### R5. Pool × streaming interaction
compio-postgres pool + a handler that streams DB query results — two code paths with their own timeouts and drop semantics. No combined test today.

**How to test:** write a handler that queries PG and yields rows; run the bench against it.

---

## Evidence from the 10 s × 10 scenario benchmark (yesterday)

Two consecutive runs at 16 workers × 300 connections:

- Zero errors in zerobench output for any scenario.
- Throughput variance ≤ 3.5 % run-to-run.
- p99 latencies tight (under 1 ms for most scenarios).
- No dropout patterns that would indicate crash-restart cycles.

**Conclusion:** the runtime does not crash in a 100-second benchmark under well-formed traffic. That is a weak form of stability; B1/B2/R1/R2/R3/R4/R5 remain real risks at longer horizons and wider traffic shapes.

---

## Recommended fix priority

| # | Fix | LOC | Justification |
|---|---|---|---|
| 1 | **B2** Wrap `.detach()` bodies in `catch_unwind` + log | ~30 | Unobservable failures are the worst kind. Single highest-leverage change. |
| 2 | **B6** Fix `runtime.rs:638` unsafe_op warning | ~10 | Forces audit of a raw pointer deref; Rust 2024 readiness. |
| 3 | **B1** `next_stream_id: u32` → `u64` | ~4 | 12-hour-at-100k-ops silent-collision bug. Trivial fix. |
| 4 | **B3** Reject non-array args body with 400 | ~5 | Wire contract enforcement; loud failure. |
| 5 | **B4** Malformed JSON → 400 not 500 | ~3 | Correct HTTP semantics. |
| 6 | **B5** Strip internal parser messages from error body | ~5 | Minor info disclosure. |

Total: ~60 LOC. All low-risk mechanical changes. Can be one commit.

---

## Recommended verification for remaining risks

In priority order:

1. **30-minute soak** at 100k req/s single-worker. RSS sampled every minute. Kills B1-adjacent concerns if wraps within the soak window; otherwise gets us meaningful evidence of no leaks on the fetch/streaming paths.
2. **Code-critic review of the Solution 1 diff** (ee2fd5f → now). Covers R1-R3. Earlier commits got this treatment; Solution 1 did not.
3. **Chaos test: kill nginx mid-bench**. Observe whether fetchEcho scenario recovers cleanly vs leaks stream_ids / connection-pool entries.
4. **HMR × in-flight stress test.** Covers R3.

Each is 1–2 hours of work. Collectively they'd move the verdict from "stable in known-good benchmarks" to "reasonable production baseline."

---

## TL;DR

**Will the runtime crash in the benchmark?** No — two 100-second runs, zero errors, bounded RSS, survives adversarial inputs without crashing.

**Is it production-stable?** Not proven. Six small real bugs (B1–B6, ~60 LOC to fix) plus five untested risk areas (R1–R5). Priority fix: wrap `.detach()` bodies in `catch_unwind` — without it, one latent panic anywhere in the fetch/pump/connection paths silently kills work with no log.
