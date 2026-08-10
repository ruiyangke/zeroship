# Runtime limits

The worker-facing per-app limit shape is `AppRuntimeLimits` in [crates/core/src/types.rs](../../crates/core/src/types.rs):

- `cpu_limit_ms`
- `wall_timeout_ms`
- `heap_limit_mb`

The runtime-side shape is `RuntimeLimits` in [crates/runtime/src/core/runtime.rs](../../crates/runtime/src/core/runtime.rs):

- `cpu_limit`
- `wall_timeout`
- `heap_limit_bytes`

Idle GC is separate. It is a `RuntimeBuilder` knob, not part of `AppRuntimeLimits`.

## Current knobs

| Knob | Where it lives | Struct default |
| --- | --- | --- |
| `cpu_limit_ms` | `AppRuntimeLimits` → `RuntimeLimits.cpu_limit` | unset |
| `wall_timeout_ms` | `AppRuntimeLimits` → `RuntimeLimits.wall_timeout` | unset |
| `heap_limit_mb` | `AppRuntimeLimits` → `RuntimeLimits.heap_limit_bytes` | 128 MB when unset |
| `idle_gc_after_ms(ms)` | `RuntimeBuilder` only | 30 s |

**"Struct default" is not what your app gets.** Those are the `Default` impl's
values for the Rust type. A deployed app is given its PLAN's limits, and an app
whose plan row is missing or whose `runtime_limits_json` fails to parse falls
back to the free tier rather than to unbounded — `FREE_TIER_RUNTIME_LIMITS` in
[crates/core/src/types.rs](../../crates/core/src/types.rs) exists precisely so
"the worker therefore never gets `(None, None, None)` (unbounded) for an unpriced
app".

## Effective limits per plan

From `builtin_plans` in
[crates/control/src/bootstrap_console.rs](../../crates/control/src/bootstrap_console.rs):

| Plan | `cpu_limit_ms` | `wall_timeout_ms` | `heap_limit_mb` |
| --- | --- | --- | --- |
| free (and the fallback for any unpriced app) | **50** | 5 000 | 64 |
| pro | 30 000 | 30 000 | 256 |
| unlimited (system/console) | none | none | none |

Two consequences worth knowing before you design around them:

- **50 ms is a CPU budget, not a wall-clock one.** Time blocked on I/O — a
  database round trip, an object fetch — does not count against it, and neither
  does time your request spends descheduled on a busy machine (enforcement is a
  POSIX timer on `CLOCK_THREAD_CPUTIME_ID`, which counts only CPU the thread
  actually consumed; see the CPU limit section below). What does count is the
  work your handler does with the bytes: parsing, copying, encoding.
- **A per-byte JavaScript pass over a 1 MiB payload does not fit.** Measured
  against a 1 MiB buffer already in memory, a byte-at-a-time loop doing a
  multiply and a few modulos costs **30–35 ms of CPU** — 60–70% of the free
  tier's whole budget, before any I/O. Streaming the same megabyte and only
  counting its length costs under 0.1 ms, so the cost is the per-byte work, not
  the size of the object. If you need to hash, transform or re-encode payloads
  of this size, size the plan for it rather than the transfer.
- **The step from free to pro is 600x**, with nothing between. If a handler
  exceeds 50 ms of CPU there is no intermediate tier to move to.

A request that exceeds its CPU budget is cancelled and surfaces
`"CPU time limit exceeded"` for that request.

## Request body size

A creator app's inbound request body is capped at **4 MiB**. It is a platform
constant, not a per-app knob: `MAX_REQUEST_BODY_BYTES` in
[crates/core/src/dispatch_frame.rs](../../crates/core/src/dispatch_frame.rs).

The gateway and the worker both enforce it, and they enforce it at two
different sizes on purpose. The gateway applies the cap to the request body it
receives. The worker receives that body wrapped in a dispatch frame - a length
prefix plus a metadata block carrying method, URL and headers - so its own
limit, `MAX_DISPATCH_FRAME_BYTES`, is the body cap plus that overhead. A worker
limit set to the body cap alone would reject requests the gateway had already
accepted.

Both tiers buffer the whole body in memory before dispatching. The cap
multiplied by in-flight concurrency is therefore the worst-case footprint,
which is what keeps this number modest. Large uploads belong in `env.storage`,
where the bytes go straight to object storage instead.

Over the cap, the caller gets `413` with a JSON body. A bare `400` from the
HTTP layer would mean the limit was applied by the framework's own default
rather than by this one.

## CPU limit

CPU enforcement is implemented in [crates/runtime/src/core/cpu_timer.rs](../../crates/runtime/src/core/cpu_timer.rs).

- Linux uses a POSIX timer on `CLOCK_THREAD_CPUTIME_ID`.
- When the timer fires, a watchdog thread calls `v8::IsolateHandle::terminate_execution()`.
- The runtime then cancels the terminating request and surfaces `"CPU time limit exceeded"` for that request.

The fast path is no-op when `cpu_limit` is unset.

## Wall timeout

Wall timeout is stored on `RuntimeLimits` and read through `Runtime::wall_timeout()` in [crates/runtime/src/core/runtime.rs](../../crates/runtime/src/core/runtime.rs). The serve path applies it while waiting for a request to finish in [crates/runtime/src/core/serve.rs](../../crates/runtime/src/core/serve.rs).

Unset means there is no runtime wall-clock cap.

## Heap limit

Heap caps are configured in [crates/runtime/src/core/runtime.rs](../../crates/runtime/src/core/runtime.rs):

- `RuntimeBuilder::heap_limit_mb(mb)` converts MB to bytes
- `RuntimeInner::new_with_plugins(...)` uses `128 * 1024 * 1024` when no cap is supplied
- a near-heap-limit callback grows the cap modestly and terminates execution after 5 consecutive hits

The regression tests live in [crates/runtime/tests/heap_limits.rs](../../crates/runtime/tests/heap_limits.rs).

## Idle GC

Idle GC is builder-only in [crates/runtime/src/core/runtime.rs](../../crates/runtime/src/core/runtime.rs):

- `RuntimeBuilder::idle_gc_after_ms(ms)`
- default constant: `DEFAULT_IDLE_GC_AFTER = 30_000ms`
- `0` disables the idle-GC ticker

When the runtime stays quiet past the threshold, the ticker calls `Isolate::low_memory_notification()`. The regression tests live in [crates/runtime/tests/idle_gc.rs](../../crates/runtime/tests/idle_gc.rs).

## Code map

- [crates/core/src/types.rs](../../crates/core/src/types.rs) — `AppRuntimeLimits`
- [crates/runtime/src/core/runtime.rs](../../crates/runtime/src/core/runtime.rs) — `RuntimeLimits`, `RuntimeBuilder`, heap callback, idle GC
- [crates/runtime/src/core/cpu_timer.rs](../../crates/runtime/src/core/cpu_timer.rs) — Linux CPU timer plumbing
- [crates/runtime/src/core/serve.rs](../../crates/runtime/src/core/serve.rs) — wall-timeout enforcement in the serve path
