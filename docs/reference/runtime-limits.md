# Runtime limits

Per-app guardrails on the V8 runtime. The control plane stores a
limits triple per app (`AppRuntimeLimits` in `crates/core/src/types.rs`);
the worker maps it onto `RuntimeLimits` (`crates/runtime/src/core/runtime.rs`)
when it builds an isolate via `Runtime::builder()`.

All three limits are **opt-in**. Unset means "use the runtime default".

| Limit                | Wire field         | Builder method                       | Default       |
| -------------------- | ------------------ | ------------------------------------ | ------------- |
| Per-request CPU time | `cpu_limit_ms`     | `RuntimeBuilder::cpu_limit(d)`       | unbounded     |
| Per-request wall    | `wall_timeout_ms`  | `RuntimeBuilder::wall_timeout(d)`    | unbounded     |
| V8 heap cap          | `heap_limit_mb`    | `RuntimeBuilder::heap_limit_mb(mb)`  | 128 MB        |

All three sit on `AppRuntimeLimits` (see `crates/core/src/types.rs`),
which is part of `AppVersionInfo` — the snapshot the worker pulls from
the control plane every 5 seconds.

---

## `cpu_limit_ms` — per-request CPU time

POSIX CPU timer (Linux only). When the user's handler consumes more
than `cpu_limit_ms` of *thread-CPU* time on a single dispatch, the
runtime calls `Isolate::terminate_execution` and the request errors
out with a 500. Other in-flight requests on the same isolate are
unaffected — the timer only fires inside the V8 entry that exceeded
the budget.

- **Use case**: defend against runaway loops, regex catastrophic
  backtracking, accidental `while (true)`.
- **Tradeoff**: too low (sub-50 ms) trips on legitimate JSON parsing
  or template rendering on cold isolates. Recommend ≥ 200 ms.
- **Off platform**: `cfg(not(target_os = "linux"))` — the Linux POSIX
  timer is the only enforcement path. Other targets ignore the field.

## `wall_timeout_ms` — per-request wall-clock

Outer wall-clock budget enforced by the runtime pump. When a handler's
promise hasn't settled within `wall_timeout_ms` from dispatch start,
the pump errors the request and (best-effort) cancels in-flight
async work via the request's cancel flag.

- **Use case**: bound user-facing latency; backstop for handlers that
  await external services with no inner timeout.
- **Tradeoff**: SSE / streaming handlers that legitimately stay open
  for minutes need a higher cap or no cap. Set per-route, not globally.

## `heap_limit_mb` — V8 heap cap

Caps the isolate's old-generation + new-generation total heap. When V8
nears the cap, a near-heap-limit callback fires; after a small number
of consecutive hits the runtime calls `terminate_execution`, which
surfaces as a 500 to the in-flight request and lets the worker evict
the isolate at the next LRU sweep.

The runtime default is **128 MB** per isolate. The old hardcoded value
(512 MB) meant a fully-saturated worker (`MAX_ISOLATES = 200` × 16 ntex
threads × 512 MB) could nominally claim 1.6 TB of address space. The
default cap drops that to ~400 GB and the per-app cap below drops it
further.

### Recommended per-app caps

| App profile                          | Cap        |
| ------------------------------------ | ---------- |
| Static SSR (Hono/Astro typical)      | 64 MB      |
| Typical CRUD with `@zeroship/db`     | 64–128 MB  |
| Image processing (Sharp/PDFKit)      | 128–256 MB |
| LLM tooling, large in-memory caches  | 256–512 MB |

Free-tier apps default to 64 MB; paid plans bump to 128 MB; "pro" can
opt into 256 MB. The control plane's plan table owns this mapping.

### Tradeoffs

- **Too low**: V8 GCs incessantly trying to keep the live set under
  the cap. Throughput drops, p99 latency spikes. Below ~32 MB even
  the runtime's own bootstrap won't fit (V8 baseline + native class
  installs is ~50 MB). **Floor: 32 MB; recommended floor: 64 MB.**
- **Too high**: cap never bites. RSS grows unbounded under burst load
  and the worker's MAX_ISOLATES heuristic stops protecting RAM.
- **Right**: app's steady-state working set fits comfortably; spikes
  trigger GC, sustained pressure trips OOM rather than swap-thrashing
  the host.

### How OOM surfaces

V8's near-heap-limit callback fires when the heap is within ~80% of
`heap_limit_bytes`. The runtime's callback:

1. Logs `tracing::warn!("v8 near heap limit", hits=N, max_hits=5)`.
2. After 5 consecutive hits, logs `tracing::error!` and calls
   `Isolate::terminate_execution`.
3. The terminating allocation throws a catchable exception inside JS;
   the next microtask boundary fires the actual termination.

User code sees a `RangeError` ("Array buffer allocation failed",
"Invalid string length", or similar V8-internal messages). The
fetch handler's response is a 500 with the V8 exception message in
the JSON body. The worker's eviction sweep then reaps the isolate
on the next LRU pass.

---

## Setting limits at deploy time

The CLI exposes a single `--heap-limit-mb=` flag (`zeroship serve`,
`zeroship deploy` — see `crates/cli/src/main.rs`). The control plane's
app-update endpoint accepts `runtime: { cpu_limit_ms, wall_timeout_ms,
heap_limit_mb }` as part of an app's settings.

```bash
# Single-tenant dev
zeroship serve myapp.js --port 3000 --heap-limit-mb=64

# Per-app at deploy
curl -X PATCH https://control.zeroship.ai/api/apps/$APP_ID/runtime \
  -d '{"cpu_limit_ms": 5000, "wall_timeout_ms": 30000, "heap_limit_mb": 128}'
```

Workers reconcile the config every 5 seconds (the `/internal/versions`
poll). A change to any limit triggers a fresh isolate at the next
cache-miss for that app — existing in-flight requests on the prior
isolate complete under the old limits, then the LRU sweep reaps it.

---

## Code map

- `crates/core/src/types.rs` — `AppRuntimeLimits` wire type.
- `crates/runtime/src/core/runtime.rs` — `RuntimeLimits`, `RuntimeBuilder`,
  near-heap-limit callback, `RuntimeInner::new_with_plugins`.
- `crates/worker/src/cache.rs` — `runtime_limits_from_app` mapping.
- `crates/runtime/tests/heap_limits.rs` — heap-cap regression tests.
- `crates/runtime/src/cpu_timer.rs` — POSIX CPU-timer plumbing.
